//! SimpleBLE 传输（第三个蓝牙实现，开发者模式可选）：与 ble 模块完全相同的
//! GATT 服务/特征语义（同一组 UUID、INBOX 写 / OUTBOX 通知的全双工字节流），
//! 底层换成 SimpleBLE（simplersble，C++ 库的 Rust 绑定），中心端与外设端出自同一套 API。
//!
//! feature 开关：simplersble 依赖挂在 cargo feature `simpleble` 后面，默认关闭——
//! 默认构建不引入 CMake/C++ 原生构建，产物与未引入本模块时完全一致。
//! 未开启 feature 时本模块是空壳：simpleble_compiled() 为 false，所有入口返回
//! 「未编译」错误，app 层据此把 bt_transport=simpleble 如实回落经典蓝牙并写明原因。
//!
//! 约束与 ble 模块一致：同一时刻只维护一条对端链路（与服务层单会话模型一致）。
//! 注意：simplersble 是同步 FFI API，阻塞性调用（扫描启停、连接、写特征）
//! 一律放进 spawn_blocking 或专用线程，不堵异步运行时。

use crate::ble::FoundDevice;
use crate::TransportError;

/// 本二进制是否编译了 SimpleBLE 真实实现（feature = "simpleble"）。
pub fn simpleble_compiled() -> bool {
    cfg!(feature = "simpleble")
}

#[cfg(feature = "simpleble")]
mod imp {
    use super::{FoundDevice, TransportError};
    use crate::ble::{channel_stream, StreamEnds, ATT_CHUNK, INBOX_UUID, OUTBOX_UUID, SERVICE_UUID};
    use crate::Conn;
    use futures_util::StreamExt;
    use simplersble::{
        Adapter, CharacteristicCapability, ClientEvent, ConnectionEvent, LocalCharacteristic,
        LocalPeripheral, Peripheral, ValueChangedEvent,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    fn sble_err(e: simplersble::Error) -> TransportError {
        TransportError::Ble(e.to_string())
    }

    fn service_uuid() -> String {
        SERVICE_UUID.to_string()
    }

    fn inbox_uuid() -> String {
        INBOX_UUID.to_string()
    }

    fn outbox_uuid() -> String {
        OUTBOX_UUID.to_string()
    }

    /// 一条链路的入站字节发送端（会话级）：多个生产者（通知流 / 写事件）共用，
    /// 断链时 take 掉并 drop，Conn 读端随即收到 EOF，与 ble 模块的语义一致。
    #[derive(Clone)]
    struct SessionTx(Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>>);

    impl SessionTx {
        fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
            Self(Arc::new(Mutex::new(Some(tx))))
        }

        fn send(&self, data: Vec<u8>) {
            let guard = self.0.lock().unwrap();
            if let Some(tx) = guard.as_ref() {
                // 通道满时丢弃（与 ble 服务端 try_send 行为一致）：协议帧有校验，
                // 上层会以断链/重传收场，不会静默错乱。
                let _ = tx.try_send(data);
            }
        }

        fn close(&self) {
            self.0.lock().unwrap().take();
        }
    }

    // ================= 中心端（扫描 / 主动连接） =================

    /// SimpleBLE 中心端：持有一个系统蓝牙适配器与最近一次扫描命中的设备句柄。
    pub struct SbleCentral {
        adapter: Adapter,
        found: Arc<Mutex<HashMap<String, Peripheral>>>,
    }

    /// 取得本机第一个蓝牙适配器；没有适配器时返回明确错误。
    pub async fn sble_central() -> Result<SbleCentral, TransportError> {
        let adapter = tokio::task::spawn_blocking(|| {
            let adapters = Adapter::get_adapters().map_err(sble_err)?;
            adapters
                .into_iter()
                .next()
                .ok_or_else(|| TransportError::Ble("未检测到蓝牙适配器".into()))
        })
        .await
        .map_err(|e| TransportError::Ble(format!("蓝牙适配器初始化失败：{e}")))??;
        Ok(SbleCentral {
            adapter,
            found: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    impl SbleCentral {
        /// 开始扫描（SimpleBLE 扫描结果在 list_found 里按 BlueLink 服务 UUID 过滤）。
        pub async fn start_scan(&self) -> Result<(), TransportError> {
            let adapter = self.adapter.clone();
            tokio::task::spawn_blocking(move || adapter.scan_start())
                .await
                .map_err(|e| TransportError::Ble(format!("扫描启动失败：{e}")))?
                .map_err(sble_err)
        }

        /// 停止扫描。
        pub async fn stop_scan(&self) -> Result<(), TransportError> {
            let adapter = self.adapter.clone();
            tokio::task::spawn_blocking(move || adapter.scan_stop())
                .await
                .map_err(|e| TransportError::Ble(format!("扫描停止失败：{e}")))?
                .map_err(sble_err)
        }

        /// 当前已发现的 BlueLink 设备列表（按信号从强到弱）。
        /// 过滤口径与 ble 模块一致：只认广播服务里带 BlueLink 服务 UUID 的设备。
        pub async fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            let adapter = self.adapter.clone();
            let svc = service_uuid();
            let hits = tokio::task::spawn_blocking(move || {
                let results = adapter.scan_get_results().map_err(sble_err)?;
                let mut hits: Vec<(FoundDevice, Peripheral)> = Vec::new();
                for p in results {
                    let is_bluelink = p
                        .services()
                        .map(|svcs| {
                            svcs.iter()
                                .any(|s| s.uuid().eq_ignore_ascii_case(&svc))
                        })
                        .unwrap_or(false);
                    if !is_bluelink {
                        continue;
                    }
                    let address = p.address().unwrap_or_default();
                    let identifier = p.identifier().unwrap_or_default();
                    // 设备 ID 需在扫描会话内稳定可回查：优先蓝牙地址，缺失时用平台标识。
                    let id = if address.is_empty() {
                        identifier.clone()
                    } else {
                        address
                    };
                    if id.is_empty() {
                        continue;
                    }
                    let name = if identifier.is_empty() {
                        "未知设备".to_string()
                    } else {
                        identifier
                    };
                    hits.push((
                        FoundDevice {
                            id,
                            name,
                            rssi: p.rssi().ok(),
                        },
                        p,
                    ));
                }
                Ok::<_, TransportError>(hits)
            })
            .await
            .map_err(|e| TransportError::Ble(format!("扫描读取失败：{e}")))??;
            let mut out = Vec::with_capacity(hits.len());
            {
                let mut map = self.found.lock().unwrap();
                for (dev, peripheral) in hits {
                    map.insert(dev.id.clone(), peripheral);
                    out.push(dev);
                }
            }
            out.sort_by_key(|d| std::cmp::Reverse(d.rssi.unwrap_or(i16::MIN)));
            Ok(out)
        }

        /// 连接指定设备并订阅 OUTBOX 通知，返回可直接交给服务层握手的连接。
        pub async fn connect(&self, device_id: &str) -> Result<Conn, TransportError> {
            let peripheral = self
                .found
                .lock()
                .unwrap()
                .get(device_id)
                .cloned()
                .ok_or_else(|| {
                    TransportError::Ble("设备已不在扫描范围内，请重新扫描".into())
                })?;

            // 连接事件流要在 connect 之前订阅，否则断开事件可能漏掉。
            let mut conn_events = peripheral.on_connection_event();

            let p_conn = peripheral.clone();
            tokio::task::spawn_blocking(move || p_conn.connect())
                .await
                .map_err(|e| TransportError::Ble(format!("连接失败：{e}")))?
                .map_err(sble_err)?;

            // 服务发现：确认对方确实提供 BlueLink 服务与 INBOX/OUTBOX 特征。
            let svc = service_uuid();
            let inbox_s = inbox_uuid();
            let outbox_s = outbox_uuid();
            let services = peripheral.services().map_err(sble_err)?;
            let service = services
                .iter()
                .find(|s| s.uuid().eq_ignore_ascii_case(&svc))
                .ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 服务".into()))?;
            let chars = service.characteristics();
            let inbox = chars
                .iter()
                .find(|c| c.uuid().eq_ignore_ascii_case(&inbox_s))
                .ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 写入特征".into()))?;
            let outbox = chars
                .iter()
                .find(|c| c.uuid().eq_ignore_ascii_case(&outbox_s))
                .ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 通知特征".into()))?;
            if !outbox.can_notify() {
                return Err(TransportError::Ble("对方通知特征不可订阅".into()));
            }
            // 无响应写吞吐更高；不支持时回落带响应写（与 ble 模块同一口径）。
            let write_command = inbox.can_write_command();
            if !write_command && !inbox.can_write_request() {
                return Err(TransportError::Ble("对方写入特征不可写".into()));
            }

            let mut values = peripheral.notify(&svc, &outbox_s).map_err(sble_err)?;

            let (stream, ends) = channel_stream();
            let StreamEnds {
                incoming_tx,
                outgoing_rx,
            } = ends;
            let session = SessionTx::new(incoming_tx);

            // 入站泵：OUTBOX 通知字节 → 流
            let in_session = session.clone();
            tokio::spawn(async move {
                while let Some(ValueChangedEvent::ValueUpdated(data)) = values.next().await {
                    in_session.send(data);
                }
                in_session.close();
            });

            // 断链监视：对端断开即关闭会话，Conn 读端收到 EOF
            let disc_session = session.clone();
            tokio::spawn(async move {
                while let Some(event) = conn_events.next().await {
                    if matches!(event, ConnectionEvent::Disconnected) {
                        disc_session.close();
                        break;
                    }
                }
            });

            // 出站泵：流字节 → std 通道 → 专用线程按 ATT_CHUNK 切片写 INBOX
            // （simplersble 写是同步 FFI，不能堵异步运行时）。
            let (std_tx, std_rx) = std::sync::mpsc::channel::<Vec<u8>>();
            let mut outgoing_rx = outgoing_rx;
            tokio::spawn(async move {
                while let Some(bytes) = outgoing_rx.recv().await {
                    if std_tx.send(bytes).is_err() {
                        break;
                    }
                }
            });
            let p_write = peripheral.clone();
            let w_session = session.clone();
            std::thread::spawn(move || {
                while let Ok(bytes) = std_rx.recv() {
                    let mut failed = false;
                    for piece in bytes.chunks(ATT_CHUNK) {
                        let data = piece.to_vec();
                        let r = if write_command {
                            p_write.write_command(&svc, &inbox_s, &data)
                        } else {
                            p_write.write_request(&svc, &inbox_s, &data)
                        };
                        if r.is_err() {
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                }
                let _ = p_write.disconnect();
                w_session.close();
            });

            Ok(Conn::new(stream, format!("simpleble:{device_id}")))
        }
    }

    // ================= 服务端（广播 / 被连接） =================

    /// SimpleBLE 服务端句柄：广播中，其他设备可扫描到本机并连入。
    /// 必须持有 LocalPeripheral 与两个特征的克隆：SimpleBLE 规定
    /// LocalPeripheral 的最后一个克隆被丢弃时外设即停止。
    pub struct SbleServer {
        local: LocalPeripheral,
        local_name: String,
        // 仅为保活（服务/特征的注册状态由 LocalPeripheral 内部持有，
        // 这里再持一份克隆防提前释放）。
        _inbox: LocalCharacteristic,
        _outbox: LocalCharacteristic,
    }

    impl SbleServer {
        /// 本机广播名。
        pub fn local_name(&self) -> &str {
            &self.local_name
        }

        /// 停止广播。
        pub async fn stop(&self) {
            let local = self.local.clone();
            let _ = tokio::task::spawn_blocking(move || local.stop()).await;
        }
    }

    /// 启动 SimpleBLE 服务端：注册与 ble 模块相同的服务/特征、开始广播。
    /// 返回句柄与入站连接接收端：每个连入并订阅 OUTBOX 的对端产生一个 Conn。
    pub async fn start_sble_server(
        local_name: &str,
    ) -> Result<(SbleServer, mpsc::Receiver<Conn>), TransportError> {
        // 建服务/特征、订阅事件流、启动广播都是同步 FFI，一并放进阻塞线程；
        // 事件流（futures Stream）本身只是通道订阅，在阻塞线程里创建后移交异步任务轮询。
        let setup = tokio::task::spawn_blocking(move || {
            let adapters = Adapter::get_adapters().map_err(sble_err)?;
            let adapter = adapters
                .into_iter()
                .next()
                .ok_or_else(|| TransportError::Ble("未检测到蓝牙适配器".into()))?;
            if !adapter.is_powered().map_err(sble_err)? {
                return Err(TransportError::Ble("蓝牙未开启，请先打开系统蓝牙".into()));
            }
            let local = adapter.create_local_peripheral().map_err(sble_err)?;
            let service = local.add_service(&service_uuid()).map_err(sble_err)?;
            let inbox = service
                .add_characteristic(
                    &inbox_uuid(),
                    &[
                        CharacteristicCapability::WriteRequest,
                        CharacteristicCapability::WriteCommand,
                    ],
                )
                .map_err(sble_err)?;
            let outbox = service
                .add_characteristic(
                    &outbox_uuid(),
                    &[CharacteristicCapability::Notify, CharacteristicCapability::Read],
                )
                .map_err(sble_err)?;
            // 读 OUTBOX 时回空值（与 ble 模块的 ReadRequest 行为一致）。
            let _ = outbox.set_callback_on_read(Vec::new);
            // 事件流必须在 start 之前订阅，否则订阅/断开事件会漏。
            let client_events = local.on_client_event();
            let inbox_events = inbox.on_event();
            let outbox_events = outbox.on_event();
            local
                .add_advertised_service(&service_uuid())
                .map_err(sble_err)?;
            local.set_connectable(true).map_err(sble_err)?;
            local.start().map_err(sble_err)?;
            Ok::<_, TransportError>((
                local,
                inbox,
                outbox,
                client_events,
                inbox_events,
                outbox_events,
            ))
        })
        .await
        .map_err(|e| TransportError::Ble(format!("SimpleBLE 服务端初始化失败：{e}")))??;
        let (local, inbox, outbox, mut client_events, mut inbox_events, mut outbox_events) = setup;

        let (inbound_tx, inbound_rx) = mpsc::channel::<Conn>(4);
        // 当前链路：会话发送端 + 当前客户端地址（仅用于 Conn 的 peer_addr 描述）
        let link: Arc<Mutex<Option<SessionTx>>> = Arc::new(Mutex::new(None));
        let client_addr: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // 客户端连接/断开事件：断开即拆链（单链路模型）
        {
            let link = link.clone();
            let client_addr = client_addr.clone();
            tokio::spawn(async move {
                while let Some(event) = client_events.next().await {
                    match event {
                        ClientEvent::Connected(addr) => {
                            *client_addr.lock().unwrap() = Some(addr);
                        }
                        ClientEvent::Disconnected(_) => {
                            *client_addr.lock().unwrap() = None;
                            if let Some(session) = link.lock().unwrap().take() {
                                session.close();
                            }
                        }
                    }
                }
            });
        }

        // OUTBOX 事件：订阅建立链路，取消订阅即断链（与 ble 模块同一口径）
        {
            let link = link.clone();
            let client_addr = client_addr.clone();
            let outbox_pump = outbox.clone();
            tokio::spawn(async move {
                use simplersble::LocalCharacteristicEvent as Ev;
                while let Some(event) = outbox_events.next().await {
                    match event {
                        Ev::Subscribed => {
                            if link.lock().unwrap().is_some() {
                                continue; // 已有链路，忽略重复订阅
                            }
                            let (stream, ends) = channel_stream();
                            let StreamEnds {
                                incoming_tx,
                                mut outgoing_rx,
                            } = ends;
                            let session = SessionTx::new(incoming_tx);
                            *link.lock().unwrap() = Some(session.clone());
                            // 出站泵：流字节 → 按 ATT_CHUNK 切片 set_value 通知对端
                            let out_char = outbox_pump.clone();
                            let w_session = session.clone();
                            tokio::spawn(async move {
                                while let Some(bytes) = outgoing_rx.recv().await {
                                    let mut failed = false;
                                    for piece in bytes.chunks(ATT_CHUNK) {
                                        if out_char.set_value(piece).is_err() {
                                            failed = true;
                                            break;
                                        }
                                    }
                                    if failed {
                                        break;
                                    }
                                }
                                w_session.close();
                            });
                            let addr = client_addr
                                .lock()
                                .unwrap()
                                .clone()
                                .unwrap_or_else(|| "peer".to_string());
                            let conn = Conn::new(stream, format!("simpleble:{addr}"));
                            if inbound_tx.send(conn).await.is_err() {
                                break;
                            }
                        }
                        Ev::Unsubscribed => {
                            if let Some(session) = link.lock().unwrap().take() {
                                session.close();
                            }
                        }
                        Ev::Written(_) => {}
                    }
                }
            });
        }

        // INBOX 事件：对端写入的字节喂给当前链路
        {
            let link = link.clone();
            tokio::spawn(async move {
                use simplersble::LocalCharacteristicEvent as Ev;
                while let Some(event) = inbox_events.next().await {
                    if let Ev::Written(data) = event {
                        let session = link.lock().unwrap().clone();
                        if let Some(session) = session {
                            session.send(data);
                        }
                    }
                }
            });
        }

        Ok((
            SbleServer {
                local,
                local_name: local_name.to_string(),
                _inbox: inbox,
                _outbox: outbox,
            },
            inbound_rx,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// SimpleBLE 路径用的 UUID 字符串必须与 ble 模块的常量逐字一致，
        /// 否则两个实现互相扫不到对方（纯字符串断言，不需要蓝牙硬件）。
        #[test]
        fn uuid字符串与ble模块一致() {
            assert_eq!(service_uuid(), "b1000001-0000-4000-8000-000000000001");
            assert_eq!(inbox_uuid(), "b1000002-0000-4000-8000-000000000001");
            assert_eq!(outbox_uuid(), "b1000003-0000-4000-8000-000000000001");
            assert_eq!(service_uuid(), crate::ble::SERVICE_UUID.to_string());
            assert_eq!(inbox_uuid(), crate::ble::INBOX_UUID.to_string());
            assert_eq!(outbox_uuid(), crate::ble::OUTBOX_UUID.to_string());
        }
    }
}

#[cfg(not(feature = "simpleble"))]
mod imp {
    use super::{FoundDevice, TransportError};
    use crate::Conn;
    use tokio::sync::mpsc;

    /// 未编译 SimpleBLE 时的统一错误：如实说明原因，app 层据此回落并提示。
    fn not_compiled() -> TransportError {
        TransportError::Ble(
            "SimpleBLE 未编译进此版本（构建时未开启 simpleble feature）".into(),
        )
    }

    /// SimpleBLE 中心端空壳（未编译）：所有操作返回「未编译」错误。
    pub struct SbleCentral;

    pub async fn sble_central() -> Result<SbleCentral, TransportError> {
        Err(not_compiled())
    }

    impl SbleCentral {
        pub async fn start_scan(&self) -> Result<(), TransportError> {
            Err(not_compiled())
        }

        pub async fn stop_scan(&self) -> Result<(), TransportError> {
            Err(not_compiled())
        }

        pub async fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            Err(not_compiled())
        }

        pub async fn connect(&self, _device_id: &str) -> Result<Conn, TransportError> {
            Err(not_compiled())
        }
    }

    /// SimpleBLE 服务端空壳（未编译）。
    pub struct SbleServer;

    impl SbleServer {
        pub fn local_name(&self) -> &str {
            ""
        }

        pub async fn stop(&self) {}
    }

    pub async fn start_sble_server(
        _local_name: &str,
    ) -> Result<(SbleServer, mpsc::Receiver<Conn>), TransportError> {
        Err(not_compiled())
    }
}

pub use imp::{sble_central, start_sble_server, SbleCentral, SbleServer};

#[cfg(test)]
mod tests {
    use super::*;

    /// feature 开关状态必须与编译配置一致：app 层的回落逻辑以此为准。
    #[test]
    fn 编译状态与feature一致() {
        assert_eq!(simpleble_compiled(), cfg!(feature = "simpleble"));
    }
}
