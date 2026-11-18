//! BLE 传输（产品通道）：每台设备同时是 GATT 服务端与中心端。
//! - 服务端：广播 BlueLink 服务，等对方连入（ble-peripheral-rust，三平台后端）。
//! - 中心端：扫描带 BlueLink 服务的设备并主动连接（btleplug）。
//!
//! 一条 GATT 连接即全双工字节流：中心端写 INBOX 特征，外围端经 OUTBOX 特征以通知回传。
//! 上层协议是长度前缀帧，跑在字节流上；ATT 层只按 ATT_CHUNK 定长分片，接收端按流重组，
//! 因此与 TCP 通道完全同构，服务层无需区分通道类型。
//! 约束：同一时刻只维护一条对端链路（与服务层单会话模型一致）。

use crate::{Conn, TransportError};
use ble_peripheral_rust::{
    gatt::{
        characteristic::Characteristic,
        peripheral_event::{
            PeripheralEvent, ReadRequestResponse, RequestResponse, WriteRequestResponse,
        },
        properties::{AttributePermission, CharacteristicProperty},
        service::Service,
    },
    Peripheral as GattPeripheral, PeripheralImpl,
};
use btleplug::api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral as CentralPeripheral};
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// BlueLink GATT 服务与特征 UUID（自有命名空间，扫描按服务过滤）。
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0xb1000001_0000_4000_8000_000000000001);
/// INBOX：中心端写入（中心 → 外围方向的数据）。
pub const INBOX_UUID: Uuid = Uuid::from_u128(0xb1000002_0000_4000_8000_000000000001);
/// OUTBOX：外围端通知（外围 → 中心方向的数据）。
pub const OUTBOX_UUID: Uuid = Uuid::from_u128(0xb1000003_0000_4000_8000_000000000001);
/// 单次 ATT 写入/通知的字节上限（主流栈协商 MTU ≥ 247 时可用 244，取 200 保守值）。
pub const ATT_CHUNK: usize = 200;

fn ble_err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError::Ble(e.to_string())
}

// ================= 通道字节流 =================

/// 字节流的平台侧端点：平台层把收到的字节推进 incoming_tx，把 outgoing_rx 的字节发出去。
pub struct StreamEnds {
    incoming_tx: mpsc::Sender<Vec<u8>>,
    outgoing_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// 基于 mpsc 通道的字节流：一端交给 Conn 当 AsyncRead/AsyncWrite，
/// 另一端（StreamEnds）由 BLE 平台泵任务驱动。字节边界不保留，与 TCP 流语义一致。
/// 发送侧用无界通道：帧本身有 16MiB 上限且发送由用户操作与文件流程节速，不会无界堆积。
pub struct ChannelStream {
    incoming: mpsc::Receiver<Vec<u8>>,
    read_buf: VecDeque<u8>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

pub fn channel_stream() -> (ChannelStream, StreamEnds) {
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(128);
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    (
        ChannelStream {
            incoming: in_rx,
            read_buf: VecDeque::new(),
            outgoing: out_tx,
        },
        StreamEnds {
            incoming_tx: in_tx,
            outgoing_rx: out_rx,
        },
    )
}

impl AsyncRead for ChannelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.read_buf.is_empty() {
            match self.incoming.poll_recv(cx) {
                Poll::Ready(Some(bytes)) => self.read_buf.extend(bytes),
                Poll::Ready(None) => return Poll::Ready(Ok(())), // 对端关闭：EOF
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = self.read_buf.len().min(buf.remaining());
        let (a, b) = self.read_buf.as_slices();
        let take_a = n.min(a.len());
        buf.put_slice(&a[..take_a]);
        if n > take_a {
            buf.put_slice(&b[..n - take_a]);
        }
        self.read_buf.drain(..n);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for ChannelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.outgoing.send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "蓝牙发送通道已关闭",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ================= 中心端（扫描 / 主动连接） =================

/// 扫描到的一台 BlueLink 设备。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FoundDevice {
    /// 平台设备 ID（不透明字符串，连接时回传）。
    pub id: String,
    /// 广播名（对方设备名）。
    pub name: String,
    /// 信号强度（dBm），平台未提供时为 None。
    pub rssi: Option<i16>,
}

/// BLE 中心端：持有一个系统蓝牙适配器。
pub struct BleCentral {
    adapter: Adapter,
}

/// 取得本机第一个蓝牙适配器；没有适配器时返回明确错误。
pub async fn ble_central() -> Result<BleCentral, TransportError> {
    let manager = Manager::new().await.map_err(ble_err)?;
    let adapters = manager.adapters().await.map_err(ble_err)?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| TransportError::Ble("未检测到蓝牙适配器".into()))?;
    Ok(BleCentral { adapter })
}

impl BleCentral {
    /// 开始扫描。不设系统级服务过滤：部分平台（Windows）的系统过滤会漏掉
    /// 服务 UUID 位于扫描响应包中的广播，导致永远扫不到；改为全量扫描，
    /// 由 list_found 按广播服务严格筛选（见下）。
    pub async fn start_scan(&self) -> Result<(), TransportError> {
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(ble_err)
    }

    /// 停止扫描。
    pub async fn stop_scan(&self) -> Result<(), TransportError> {
        self.adapter.stop_scan().await.map_err(ble_err)
    }

    /// 当前已发现的 BlueLink 设备列表（按信号从强到弱）。
    pub async fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
        let mut out = Vec::new();
        for p in self.adapter.peripherals().await.map_err(ble_err)? {
            let props = p.properties().await.map_err(ble_err)?;
            let Some(props) = props else { continue };
            // 只认广播里带 BlueLink 服务 UUID 的设备。服务列表暂时为空的设备
            // （广播包还没收全）先跳过，后续轮询收到完整广播后自然会出现。
            if !props.services.contains(&SERVICE_UUID) {
                continue;
            }
            out.push(FoundDevice {
                id: p.id().to_string(),
                name: props
                    .local_name
                    .clone()
                    .unwrap_or_else(|| "未知设备".to_string()),
                rssi: props.rssi,
            });
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.rssi.unwrap_or(i16::MIN)));
        Ok(out)
    }

    /// 连接指定设备并完成 GATT 发现，返回可直接交给服务层握手的连接。
    pub async fn connect(&self, device_id: &str) -> Result<Conn, TransportError> {
        let peripherals = self.adapter.peripherals().await.map_err(ble_err)?;
        let mut target: Option<CentralPeripheral> = None;
        for p in peripherals {
            if p.id().to_string() == device_id {
                target = Some(p);
                break;
            }
        }
        let peripheral =
            target.ok_or_else(|| TransportError::Ble("设备已不在扫描范围内，请重新扫描".into()))?;
        if !peripheral.is_connected().await.map_err(ble_err)? {
            peripheral.connect().await.map_err(ble_err)?;
        }
        peripheral.discover_services().await.map_err(ble_err)?;
        let chars = peripheral.characteristics();
        let inbox = chars
            .iter()
            .find(|c| c.uuid == INBOX_UUID)
            .cloned()
            .ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 写入特征".into()))?;
        let outbox = chars
            .iter()
            .find(|c| c.uuid == OUTBOX_UUID)
            .cloned()
            .ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 通知特征".into()))?;
        peripheral.subscribe(&outbox).await.map_err(ble_err)?;

        let (stream, ends) = channel_stream();
        let StreamEnds {
            incoming_tx,
            mut outgoing_rx,
        } = ends;

        // 入站泵：通知字节 → 流
        let mut notifications = peripheral.notifications().await.map_err(ble_err)?;
        let in_tx = incoming_tx.clone();
        tokio::spawn(async move {
            while let Some(n) = notifications.next().await {
                if n.uuid == OUTBOX_UUID && in_tx.send(n.value).await.is_err() {
                    break;
                }
            }
        });

        // 出站泵：流字节 → 按 ATT_CHUNK 切片写 INBOX（无响应写优先，吞吐更高）
        let write_type = if inbox
            .properties
            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        {
            WriteType::WithoutResponse
        } else {
            WriteType::WithResponse
        };
        let peer = peripheral.clone();
        tokio::spawn(async move {
            while let Some(bytes) = outgoing_rx.recv().await {
                let mut failed = false;
                for piece in bytes.chunks(ATT_CHUNK) {
                    if peer.write(&inbox, piece, write_type).await.is_err() {
                        failed = true;
                        break;
                    }
                }
                if failed {
                    break;
                }
            }
            let _ = peer.disconnect().await;
        });

        Ok(Conn::new(stream, format!("ble:{device_id}")))
    }
}

// ================= 服务端（广播 / 被连接） =================

/// BLE 服务端句柄：广播中，其他设备可扫描到本机并连入。
pub struct BleServer {
    peripheral: Arc<Mutex<GattPeripheral>>,
    local_name: String,
}

impl BleServer {
    /// 本机广播名。
    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    /// 蓝牙是否已开启。
    pub async fn is_powered(&self) -> bool {
        self.peripheral
            .lock()
            .await
            .is_powered()
            .await
            .unwrap_or(false)
    }

    /// 停止广播。
    pub async fn stop(&self) {
        let _ = self.peripheral.lock().await.stop_advertising().await;
    }
}

/// 启动 GATT 服务端：注册服务与特征、开始广播。
/// 返回句柄与入站连接接收端：每个连入并订阅成功的对端产生一个 Conn。
pub async fn start_ble_server(
    local_name: &str,
) -> Result<(BleServer, mpsc::Receiver<Conn>), TransportError> {
    let (event_tx, mut event_rx) = mpsc::channel::<PeripheralEvent>(128);
    let mut peripheral = GattPeripheral::new(event_tx).await.map_err(ble_err)?;
    if !peripheral.is_powered().await.map_err(ble_err)? {
        return Err(TransportError::Ble("蓝牙未开启，请先打开系统蓝牙".into()));
    }
    peripheral
        .add_service(&Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: INBOX_UUID,
                    properties: vec![
                        CharacteristicProperty::Write,
                        CharacteristicProperty::WriteWithoutResponse,
                    ],
                    permissions: vec![AttributePermission::Writeable],
                    value: None,
                    descriptors: vec![],
                },
                Characteristic {
                    uuid: OUTBOX_UUID,
                    properties: vec![CharacteristicProperty::Notify, CharacteristicProperty::Read],
                    permissions: vec![AttributePermission::Readable],
                    value: None,
                    descriptors: vec![],
                },
            ],
        })
        .await
        .map_err(ble_err)?;
    peripheral
        .start_advertising(local_name, &[SERVICE_UUID])
        .await
        .map_err(ble_err)?;

    let peripheral = Arc::new(Mutex::new(peripheral));
    let (inbound_tx, inbound_rx) = mpsc::channel::<Conn>(4);

    // 服务端事件循环：订阅建立链路，写入喂字节，取消订阅即断链
    let pump_peripheral = peripheral.clone();
    tokio::spawn(async move {
        let mut link: Option<(String, mpsc::Sender<Vec<u8>>)> = None;
        while let Some(event) = event_rx.recv().await {
            match event {
                PeripheralEvent::CharacteristicSubscriptionUpdate {
                    request,
                    subscribed,
                } if request.characteristic == OUTBOX_UUID => {
                    if subscribed {
                        let (stream, ends) = channel_stream();
                        let StreamEnds {
                            incoming_tx,
                            mut outgoing_rx,
                        } = ends;
                        link = Some((request.client.clone(), incoming_tx));
                        let out_peripheral = pump_peripheral.clone();
                        tokio::spawn(async move {
                            while let Some(bytes) = outgoing_rx.recv().await {
                                let mut p = out_peripheral.lock().await;
                                let mut failed = false;
                                for piece in bytes.chunks(ATT_CHUNK) {
                                    if p.update_characteristic(OUTBOX_UUID, piece.to_vec())
                                        .await
                                        .is_err()
                                    {
                                        failed = true;
                                        break;
                                    }
                                }
                                if failed {
                                    break;
                                }
                            }
                        });
                        let conn = Conn::new(stream, format!("ble:{}", request.client));
                        if inbound_tx.send(conn).await.is_err() {
                            break;
                        }
                    } else if link
                        .as_ref()
                        .map(|(c, _)| c == &request.client)
                        .unwrap_or(false)
                    {
                        link = None; // 发送端随之丢弃，读端收到 EOF，服务层结束会话
                    }
                }
                PeripheralEvent::WriteRequest {
                    request,
                    value,
                    responder,
                    ..
                } => {
                    let target = link.as_ref().filter(|(client, _)| {
                        request.characteristic == INBOX_UUID && client == &request.client
                    });
                    if let Some((_, tx)) = target {
                        let _ = tx.try_send(value);
                    }
                    let _ = responder.send(WriteRequestResponse {
                        response: RequestResponse::Success,
                    });
                }
                PeripheralEvent::ReadRequest { responder, .. } => {
                    let _ = responder.send(ReadRequestResponse {
                        value: Vec::new(),
                        response: RequestResponse::Success,
                    });
                }
                PeripheralEvent::StateUpdate { is_powered: false } => {
                    link = None;
                }
                _ => {}
            }
        }
    });

    Ok((
        BleServer {
            peripheral,
            local_name: local_name.to_string(),
        },
        inbound_rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bluelink_protocol::{Control, Frame};

    /// 通道字节流经定长分片泵后仍能无损承载协议帧（含 300KB 大帧）。
    #[tokio::test]
    async fn 通道流分片重组无损() {
        let (stream_a, ends_a) = channel_stream();
        let (stream_b, ends_b) = channel_stream();
        // 交叉泵：A 出 → 64 字节分片 → B 入；B 出 → A 入
        let mut a_out = ends_a.outgoing_rx;
        let b_in = ends_b.incoming_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = a_out.recv().await {
                for piece in bytes.chunks(64) {
                    if b_in.send(piece.to_vec()).await.is_err() {
                        return;
                    }
                }
            }
        });
        let mut b_out = ends_b.outgoing_rx;
        let a_in = ends_a.incoming_tx.clone();
        tokio::spawn(async move {
            while let Some(bytes) = b_out.recv().await {
                for piece in bytes.chunks(64) {
                    if a_in.send(piece.to_vec()).await.is_err() {
                        return;
                    }
                }
            }
        });
        let a = Conn::new(stream_a, "ble:test-a".into());
        let b = Conn::new(stream_b, "ble:test-b".into());
        a.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), Frame::Control(Control::Ping));
        let big = Frame::Chunk(bluelink_protocol::FileChunk {
            transfer_id: "12345678-1234-1234-1234-1234567890ab".to_string(),
            offset: 0,
            compressed: false,
            data: vec![9u8; 300 * 1024],
        });
        b.send(&big).await.unwrap();
        assert_eq!(a.recv().await.unwrap(), big);
    }
}
