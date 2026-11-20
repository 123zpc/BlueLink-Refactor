//! BlueLink 补丁（vendored fork，见 third-party/ble-peripheral-rust/README.bluelink.md）：
//! 本文件里的处理器都是 WinRT 经 FFI 回调进来的 TypedEventHandler 闭包，运行在
//! 系统线程上。原版在此对大量可失败的 WinRT 调用直接 unwrap：一旦调用失败
//! （对端在订阅/读写过程中断开、GATT 会话正在拆除等真实场景都会触发），
//! panic 会穿越 FFI 边界导致整个进程 abort（真机实证：对端 BlueLink 被连入时
//! 直接闪退）。本补丁把回调内全部 unwrap 改为防御式处理：失败只记 log 并
//! 提前返回，绝不让回调 panic。事件语义与原版保持一致。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::gatt::peripheral_event::{
    PeripheralEvent, PeripheralRequest, ReadRequestResponse, RequestResponse, WriteRequestResponse,
};
use crate::peripheral::winrt::win_utils::{
    buffer_to_vec, device_id_from_session, to_uuid, vec_to_buffer,
};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use uuid::Uuid;
use windows::core::IInspectable;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattProtocolError, GattServiceProviderAdvertisementStatus, GattSession, GattSubscribedClient,
};
use windows::Devices::Radios::{Radio, RadioState};
use windows::Foundation::Collections::IVectorView;
use windows::{
    Devices::Bluetooth::GenericAttributeProfile::{
        GattLocalCharacteristic, GattReadRequestedEventArgs, GattServiceProvider,
        GattServiceProviderAdvertisementStatusChangedEventArgs, GattWriteRequestedEventArgs,
    },
    Foundation::TypedEventHandler,
};

/// 从会话取对端设备 ID：会话已失效时返回空串（与 device_id_from_session
/// 本身的兜底口径一致），不 panic。
fn client_id_of(session: windows::core::Result<GattSession>) -> String {
    match session {
        Ok(s) => device_id_from_session(s),
        Err(e) => {
            log::error!("Failed to get GATT session for client id: {}", e);
            String::new()
        }
    }
}

pub struct WinEventHandler {
    sender_tx: Sender<PeripheralEvent>,
    connected_clients: Arc<RwLock<HashMap<(Uuid, Uuid), Vec<String>>>>,
}

impl WinEventHandler {
    pub fn new(sender_tx: Sender<PeripheralEvent>) -> Self {
        Self {
            sender_tx,
            connected_clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create_radio_listener(&self) -> TypedEventHandler<Radio, IInspectable> {
        let sender_tx: Sender<PeripheralEvent> = self.sender_tx.clone();

        return TypedEventHandler::new(
            move |originator: &Option<Radio>, _: &Option<IInspectable>| {
                let Some(radio) = originator.as_ref() else {
                    return Ok(());
                };
                let Ok(state) = radio.State() else {
                    return Ok(());
                };
                let is_on = state == RadioState::On;
                futures::executor::block_on(async {
                    if let Err(err) = sender_tx
                        .send(PeripheralEvent::StateUpdate { is_powered: is_on })
                        .await
                    {
                        log::error!("Error sending delegate event: {}", err);
                    }
                });
                Ok(())
            },
        );
    }

    pub fn create_advertisement_status_handler(
        &self,
    ) -> TypedEventHandler<
        GattServiceProvider,
        GattServiceProviderAdvertisementStatusChangedEventArgs,
    > {
        TypedEventHandler::new(move |originator: &Option<GattServiceProvider>, args: &Option<GattServiceProviderAdvertisementStatusChangedEventArgs>| {
            let (Some(service), Some(event_args)) = (originator.as_ref(), args.as_ref()) else {
                return Ok(());
            };
            let status = event_args.Status()?;
            if let Ok(uuid) = service.Service().and_then(|s| s.Uuid()) {
                log::debug!("Advertisement Status: {:?}: Started: {:?}",
                    to_uuid(&uuid),
                    status == GattServiceProviderAdvertisementStatus::Started);
            }
            Ok(())
        })
    }

    pub fn create_subscribe_handler(
        &self,
        service_uuid: Uuid,
    ) -> TypedEventHandler<GattLocalCharacteristic, IInspectable> {
        let connected_clients = Arc::clone(&self.connected_clients);
        let sender_tx: Sender<PeripheralEvent> = self.sender_tx.clone();

        TypedEventHandler::new(
            move |originator: &Option<GattLocalCharacteristic>, _: &Option<IInspectable>| {
                let Some(characteristic) = originator.as_ref() else {
                    return Ok(());
                };
                let Ok(char_guid) = characteristic.Uuid() else {
                    return Ok(());
                };
                let characteristic_uuid = to_uuid(&char_guid);

                // 对端在断开过程中订阅状态变化时 SubscribedClients 会失败：
                // 记日志并放弃本次 diff（后续断开事件会把状态收尾），不 panic。
                let subscribed_clients: IVectorView<GattSubscribedClient> =
                    match characteristic.SubscribedClients() {
                        Ok(clients) => clients,
                        Err(e) => {
                            log::error!("Failed to read subscribed clients: {}", e);
                            return Ok(());
                        }
                    };

                let new_clients: Vec<String> = subscribed_clients
                    .into_iter()
                    .filter_map(|client| client.Session().ok())
                    .map(device_id_from_session)
                    .collect();

                // RwLock 若曾被毒化也继续用内部数据，不让锁问题变成回调 panic。
                let mut old_clients_store = connected_clients
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut added_clients: Vec<String> = Vec::new();
                let mut removed_clients: Vec<String> = Vec::new();

                if let Some(old_clients) = old_clients_store
                    .get_mut(&(service_uuid, characteristic_uuid))
                {
                    for client in &new_clients {
                        if !old_clients.contains(client) {
                            added_clients.push(client.clone());
                        }
                    }
                    for client in old_clients.clone() {
                        if !new_clients.contains(&client) {
                            removed_clients.push(client.clone());
                        }
                    }

                    *old_clients = new_clients;
                } else {
                    old_clients_store
                        .insert((service_uuid, characteristic_uuid), new_clients.clone());
                    added_clients.extend(new_clients.clone());
                }

                // Update Newly added/removed clients
                futures::executor::block_on(async {
                    for client in added_clients {
                        if let Err(err) = sender_tx
                            .send(PeripheralEvent::CharacteristicSubscriptionUpdate {
                                request: PeripheralRequest {
                                    client,
                                    service: service_uuid,
                                    characteristic: characteristic_uuid,
                                },
                                subscribed: true,
                            })
                            .await
                        {
                            log::error!("Error sending delegate event: {}", err);
                        }
                    }

                    for client in removed_clients {
                        if let Err(err) = sender_tx
                            .send(PeripheralEvent::CharacteristicSubscriptionUpdate {
                                request: PeripheralRequest {
                                    client,
                                    service: service_uuid,
                                    characteristic: characteristic_uuid,
                                },
                                subscribed: false,
                            })
                            .await
                        {
                            log::error!("Error sending delegate event: {}", err);
                        }
                    }
                });
                Ok(())
            },
        )
    }

    pub fn create_read_handler(
        &mut self,
        service_uuid: Uuid,
    ) -> TypedEventHandler<GattLocalCharacteristic, GattReadRequestedEventArgs> {
        let sender_tx: Sender<PeripheralEvent> = self.sender_tx.clone();

        TypedEventHandler::new(
            move |originator: &Option<GattLocalCharacteristic>,
                  args: &Option<GattReadRequestedEventArgs>| {
                let (Some(event_args), Some(characteristic)) =
                    (args.as_ref(), originator.as_ref())
                else {
                    return Ok(());
                };

                futures::executor::block_on(async {
                    // 对端在请求完成前断开时 GetRequestAsync 及其 await 都可能
                    // 失败：放弃这次读请求即可，不 panic。
                    let Ok(op) = event_args.GetRequestAsync() else {
                        return;
                    };
                    let Ok(request) = op.await else {
                        return;
                    };
                    {
                        // let mtu = event_args.Session().unwrap().MaxPduSize().unwrap();
                        let (resp_tx, resp_rx) = oneshot::channel::<ReadRequestResponse>();
                        let Ok(char_guid) = characteristic.Uuid() else {
                            return;
                        };
                        if let Err(e) = sender_tx
                            .send(PeripheralEvent::ReadRequest {
                                request: PeripheralRequest {
                                    client: client_id_of(event_args.Session()),
                                    service: service_uuid,
                                    characteristic: to_uuid(&char_guid),
                                },
                                offset: request.Offset().unwrap_or(0) as u64,
                                responder: resp_tx,
                            })
                            .await
                        {
                            log::error!("Error sending delegate event: {}", e);
                            return;
                        }

                        if let Ok(result) = resp_rx.await {
                            if result.response == RequestResponse::Success {
                                let _ = request.RespondWithValue(&vec_to_buffer(result.value));
                                return;
                            }
                            let _ = request
                                .RespondWithProtocolError(result.response.to_gatt_protocol_error());
                            return;
                        }

                        if let Ok(code) = GattProtocolError::UnlikelyError() {
                            let _ = request.RespondWithProtocolError(code);
                        }
                    }
                });

                return Ok(());
            },
        )
    }

    pub fn create_write_handler(
        &self,
        service_uuid: Uuid,
    ) -> TypedEventHandler<GattLocalCharacteristic, GattWriteRequestedEventArgs> {
        let sender_tx = self.sender_tx.clone();

        TypedEventHandler::new(
            move |originator: &Option<GattLocalCharacteristic>,
                  args: &Option<GattWriteRequestedEventArgs>| {
                let (Some(event_args), Some(characteristic)) =
                    (args.as_ref(), originator.as_ref())
                else {
                    return Ok(());
                };
                futures::executor::block_on(async {
                    // 对端在写请求完成前断开时整条链都可能失败：放弃即可。
                    let Ok(op) = event_args.GetRequestAsync() else {
                        return;
                    };
                    let Ok(request) = op.await else {
                        return;
                    };
                    {
                        // let offset = request.Offset().unwrap();
                        // let mtu = event_args.Session().unwrap().MaxPduSize().unwrap();
                        let (resp_tx, resp_rx) = oneshot::channel::<WriteRequestResponse>();
                        let Ok(char_guid) = characteristic.Uuid() else {
                            return;
                        };
                        let char_uuid = to_uuid(&char_guid);
                        let Ok(raw_value) = request.Value() else {
                            return;
                        };
                        if let Err(e) = sender_tx
                            .send(PeripheralEvent::WriteRequest {
                                request: PeripheralRequest {
                                    client: client_id_of(event_args.Session()),
                                    service: service_uuid,
                                    characteristic: char_uuid,
                                },
                                value: buffer_to_vec(&raw_value),
                                offset: request.Offset().unwrap_or(0) as u64,
                                responder: resp_tx,
                            })
                            .await
                        {
                            log::error!("Error sending delegate event: {}", e);
                            return;
                        }

                        if let Ok(result) = resp_rx.await {
                            if result.response == RequestResponse::Success {
                                // 对端已断开时 Respond 会失败：忽略，不 panic。
                                let _ = request.Respond();
                                return;
                            }
                            let _ = request
                                .RespondWithProtocolError(result.response.to_gatt_protocol_error());
                            return;
                        }

                        if let Ok(code) = GattProtocolError::UnlikelyError() {
                            let _ = request.RespondWithProtocolError(code);
                        }
                    }
                });

                return Ok(());
            },
        )
    }
}

impl RequestResponse {
    fn to_gatt_protocol_error(self) -> u8 {
        let result = match self {
            RequestResponse::Success => Ok(0),
            RequestResponse::InvalidHandle => GattProtocolError::InvalidHandle(),
            RequestResponse::RequestNotSupported => GattProtocolError::RequestNotSupported(),
            RequestResponse::InvalidOffset => GattProtocolError::InvalidOffset(),
            RequestResponse::UnlikelyError => GattProtocolError::UnlikelyError(),
        };
        if let Ok(value) = result {
            return value;
        }
        return 0;
    }
}
