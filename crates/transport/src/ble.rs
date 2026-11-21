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
/// 字段对 crate 内开放：经典蓝牙（classic 模块）的 socket 泵同样走这套通道流。
/// outgoing 的每条消息附带一个出站队列许可（见 OUT_QUEUE_FRAMES）：泵取走
/// 消息时许可随消息一起被丢弃、槽位归还，写端由此获得背压。
pub struct StreamEnds {
    pub(crate) incoming_tx: mpsc::Sender<Vec<u8>>,
    pub(crate) outgoing_rx: mpsc::UnboundedReceiver<(Vec<u8>, tokio::sync::OwnedSemaphorePermit)>,
}

/// 出站队列深度（帧数）：有界是刻意的（v2.6.2）——旧版无界队列把
/// 「已交给传输层」冒充「已发出」，BLE 上 42KB 的图实际还在空中慢慢
/// 滴灌时发送端就报了 100%、把整段传输尾巴误标成「等待校验」。
/// 有界后 Conn::send 在队列满时自然等待（背压），进度与状态才诚实；
/// 深度 8 足够让泵不饿死，又把「排队未发」压在最多 8 帧内。
/// 实现：tokio mpsc 没有 poll_reserve，改走信号量——写端先拿许可再
/// 入队，许可随消息旅行、泵取走消息时释放。
pub const OUT_QUEUE_FRAMES: usize = 8;

/// 基于 mpsc 通道的字节流：一端交给 Conn 当 AsyncRead/AsyncWrite，
/// 另一端（StreamEnds）由 BLE 平台泵任务驱动。字节边界不保留，与 TCP 流语义一致。
pub struct ChannelStream {
    incoming: mpsc::Receiver<Vec<u8>>,
    read_buf: VecDeque<u8>,
    /// 写端发送通道；shutdown 时置 None 关掉，平台出站泵随之结束并拆链。
    outgoing: Option<mpsc::UnboundedSender<(Vec<u8>, tokio::sync::OwnedSemaphorePermit)>>,
    /// 出站队列槽位信号量（与泵共享同一实例）。
    out_slots: Arc<tokio::sync::Semaphore>,
    /// 正在等待槽位的 acquire future（poll_write 跨调用保留，负责登记唤醒）。
    out_acquire: Option<
        std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            tokio::sync::OwnedSemaphorePermit,
                            tokio::sync::AcquireError,
                        >,
                    > + Send,
            >,
        >,
    >,
}

pub fn channel_stream() -> (ChannelStream, StreamEnds) {
    let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(128);
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let slots = Arc::new(tokio::sync::Semaphore::new(OUT_QUEUE_FRAMES));
    (
        ChannelStream {
            incoming: in_rx,
            read_buf: VecDeque::new(),
            outgoing: Some(out_tx),
            out_slots: slots,
            out_acquire: None,
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
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // 有界队列背压：先等一个出站槽位（future 登记唤醒、队列有空位
        // 时自动续写），拿到许可才把字节连同许可一起入队；泵取走消息
        // 时许可被丢弃、槽位归还。发送方由此被真实发送节奏节速。
        if self.outgoing.is_none() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "蓝牙发送通道已关闭",
            )));
        }
        if self.out_acquire.is_none() {
            let slots = self.out_slots.clone();
            self.out_acquire = Some(Box::pin(async move { slots.acquire_owned().await }));
        }
        let permit = {
            let fut = self.out_acquire.as_mut().unwrap();
            match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(p)) => {
                    self.out_acquire = None;
                    p
                }
                Poll::Ready(Err(_)) => {
                    self.out_acquire = None;
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "蓝牙发送通道已关闭",
                    )));
                }
                Poll::Pending => return Poll::Pending,
            }
        };
        match self.outgoing.as_ref().unwrap().send((buf.to_vec(), permit)) {
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

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // 关掉发送通道：平台出站泵读到通道结束即拆链（BLE 断开、经典关 socket）
        self.outgoing = None;
        Poll::Ready(Ok(()))
    }
}

// ================= ATT 切片序号层（BLE / SimpleBLE 共用） =================

/// 切片头魔数（'S'）：每片 = [魔数 1B][序号 u16 LE][载荷]。
pub(crate) const SLICE_MAGIC: u8 = 0x53;
/// 切片头长度。
pub(crate) const SLICE_HEADER: usize = 3;
/// 单片载荷上限（ATT_CHUNK 含头）。
pub(crate) const SLICE_DATA: usize = ATT_CHUNK - SLICE_HEADER;

/// 切片编码器（每条链路一个方向一个）：给每片盖递增序号。
/// 旧版裸切片无序号：空中丢一片/重一片，接收端字节流悄悄错位，
/// 直到帧长读出天文数字才整条连接暴毙（2026-11-21 真机实锤）。
#[derive(Debug)]
pub(crate) struct SliceEncoder {
    seq: u16,
}

impl SliceEncoder {
    pub fn new() -> Self {
        Self { seq: 0 }
    }

    /// 把一段字节切成带序号的片（空输入不产片）。
    pub fn encode(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        data.chunks(SLICE_DATA)
            .map(|piece| {
                let mut v = Vec::with_capacity(SLICE_HEADER + piece.len());
                v.push(SLICE_MAGIC);
                v.extend_from_slice(&self.seq.to_le_bytes());
                v.extend_from_slice(piece);
                self.seq = self.seq.wrapping_add(1);
                v
            })
            .collect()
    }
}

/// 切片解码器：按序号验收每一片。
/// - 重复片（序号 = 上一片）：丢弃并计数——重复字节进流必然错位。
/// - 序号跳跃（丢片）：计数并落 essential 日志，载荷照常交付——帧层
///   解码器会丢掉受损帧并重新对齐，在这一层把后续好片全扔掉只会
///   扩大损失。
/// - 头魔数不对：整片丢弃并计数（不是本协议的片或已污染）。
#[derive(Debug)]
pub(crate) struct SliceDecoder {
    expect: Option<u16>,
    tag: &'static str,
    pub gaps: u64,
    pub dups: u64,
    pub corrupt: u64,
}

impl SliceDecoder {
    pub fn new(tag: &'static str) -> Self {
        Self {
            expect: None,
            tag,
            gaps: 0,
            dups: 0,
            corrupt: 0,
        }
    }

    /// 验收一片：返回应喂入字节流的载荷；None 表示该片应丢弃。
    pub fn decode(&mut self, slice: &[u8]) -> Option<Vec<u8>> {
        if slice.len() < SLICE_HEADER || slice[0] != SLICE_MAGIC {
            self.corrupt += 1;
            if self.corrupt == 1 || self.corrupt % 100 == 0 {
                crate::probe::log(
                    self.tag,
                    &format!("切片头无效，整片丢弃（累计 {} 次）", self.corrupt),
                );
            }
            return None;
        }
        let seq = u16::from_le_bytes([slice[1], slice[2]]);
        match self.expect {
            None => self.expect = Some(seq.wrapping_add(1)),
            Some(exp) if seq == exp => self.expect = Some(exp.wrapping_add(1)),
            Some(exp) if seq == exp.wrapping_sub(1) => {
                self.dups += 1;
                crate::probe::log(
                    self.tag,
                    &format!("切片重复（序号 {seq}），丢弃（累计 {} 次）", self.dups),
                );
                return None;
            }
            Some(exp) => {
                self.gaps += 1;
                crate::probe::log(
                    self.tag,
                    &format!(
                        "切片序号跳跃：期望 {exp} 收到 {seq}（累计丢片事件 {} 次），帧层将重新对齐",
                        self.gaps
                    ),
                );
                self.expect = Some(seq.wrapping_add(1));
            }
        }
        Some(slice[SLICE_HEADER..].to_vec())
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
    /// 系统里是否已配对/已记住这台设备。目前只有经典蓝牙 Inquiry 能提供
    /// （BLUETOOTH_DEVICE_INFO 的 fRemembered/fAuthenticated），BLE 与
    /// SimpleBLE 的扫描结果没有这个信息，恒为 false（表示未知，不显示）。
    pub paired: bool,
}

/// 蓝牙连接尝试的统一超时：超过即判失败，不许无限干等（用户真机实证：
/// 点了连接既连不上又取消不掉）。经典路的阻塞 connect 在系统调用里掐不断，
/// 由 app 层令牌作废兜底；BLE/SimpleBLE 在此直接超时返回。
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 同名设备合并（BLE 与 SimpleBLE 共用）：同一台物理设备会以多个广播身份
/// 出现——Windows 的 GATT 服务端广播用轮换的随机地址，旧地址身份不会被
/// 扫描栈淘汰，于是同一台机器在列表里出现多条（用户真机实证：同名两条，
/// 一条满格一条 0 格，0 格那条是早已轮换走的旧地址、信号停在最后一次的值）。
/// 合并键 = 设备名：同名条目合成一条，位置取首次出现处，代表身份取信号
/// 最强的一条（最可能还活着、连得上），信号取该代表的最新值。
/// 「未知设备」（名字还没随扫描响应到达）不参与合并：无法证明它们与某个
/// 已命名设备是同一台，贸然合并可能把第二台真设备藏掉；名字到达后自然并入。
pub fn merge_same_name(devices: Vec<FoundDevice>) -> Vec<FoundDevice> {
    let mut out: Vec<FoundDevice> = Vec::with_capacity(devices.len());
    for d in devices {
        if d.name == "未知设备" {
            out.push(d);
            continue;
        }
        match out.iter_mut().find(|e| e.name == d.name) {
            Some(slot) => {
                // 信号更强的身份接替代表（含代表尚无信号的情况）；paired 取并集
                let stronger = match (d.rssi, slot.rssi) {
                    (Some(a), Some(b)) => a > b,
                    (Some(_), None) => true,
                    _ => false,
                };
                if stronger {
                    slot.id = d.id;
                    slot.rssi = d.rssi;
                }
                slot.paired = slot.paired || d.paired;
            }
            None => out.push(d),
        }
    }
    out
}

/// SCANPROBE（仅日志）：本轮扫描已 dump 过广播全字段的平台设备 ID 集合，
/// start_scan 时清空，保证每台设备每轮扫描只记一次。
static SCANPROBE_DUMPED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// 把一台 BLE 设备的广播全字段 dump 成 SCANPROBE 分段行（仅日志，每轮只记一次）。
/// 注意在服务过滤之前调用：诊断要看的是适配器听到的全部设备，不只是 BlueLink。
fn dump_ble_device_once(platform_id: &str, props: &btleplug::api::PeripheralProperties) {
    let addr = props.address.to_string();
    {
        let Ok(mut g) = SCANPROBE_DUMPED.lock() else {
            return;
        };
        if g.iter().any(|id| id == platform_id) {
            return;
        }
        g.push(platform_id.to_string());
    }
    let hex = |data: &[u8]| -> String { data.iter().map(|b| format!("{b:02X}")).collect() };
    let opt =
        |v: &Option<String>| -> String { v.clone().unwrap_or_else(|| "（空）".to_string()) };
    let mut lines = vec![
        format!("SCANPROBE 设备开始 [ble] {addr}"),
        format!("SCANPROBE 地址={addr}"),
        format!("SCANPROBE 平台ID={platform_id}"),
        format!(
            "SCANPROBE 地址类型={}",
            props
                .address_type
                .map(|t| format!("{t:?}"))
                .unwrap_or_else(|| "（空）".to_string())
        ),
        format!("SCANPROBE 本地名={}", opt(&props.local_name)),
        format!(
            "SCANPROBE RSSI={}",
            props
                .rssi
                .map(|v| v.to_string())
                .unwrap_or_else(|| "（空）".to_string())
        ),
        format!(
            "SCANPROBE 发射功率={}",
            props
                .tx_power_level
                .map(|v| v.to_string())
                .unwrap_or_else(|| "（空）".to_string())
        ),
        format!(
            "SCANPROBE 服务UUID={}",
            if props.services.is_empty() {
                "（空）".to_string()
            } else {
                props
                    .services
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            }
        ),
        format!(
            "SCANPROBE 厂商数据={}",
            if props.manufacturer_data.is_empty() {
                "（空）".to_string()
            } else {
                props
                    .manufacturer_data
                    .iter()
                    .map(|(id, data)| format!("0x{id:04X}:{}", hex(data)))
                    .collect::<Vec<_>>()
                    .join(";")
            }
        ),
        format!(
            "SCANPROBE 服务数据={}",
            if props.service_data.is_empty() {
                "（空）".to_string()
            } else {
                props
                    .service_data
                    .iter()
                    .map(|(u, data)| format!("{u}:{}", hex(data)))
                    .collect::<Vec<_>>()
                    .join(";")
            }
        ),
        format!(
            "SCANPROBE 设备类class={}",
            props
                .class
                .map(|v| format!("0x{v:06X}"))
                .unwrap_or_else(|| "（空）".to_string())
        ),
        format!("SCANPROBE 设备结束 [ble] {addr}"),
    ];
    for line in lines.drain(..) {
        crate::probe::push_line(line);
    }
}

/// BLE 中心端：持有一个系统蓝牙适配器。
pub struct BleCentral {
    adapter: Adapter,
    /// 首次发现序（平台 ID）：列表顺序按它稳定排列，RSSI 只原地更新数值，
    /// 不许按信号实时重排（用户真机实证：按 RSSI 每秒重排时列表上下抖动剧烈）。
    /// btleplug 的 peripherals() 顺序本身不稳定，必须自己记序。
    seen_order: std::sync::Mutex<Vec<String>>,
}

/// 取得本机第一个蓝牙适配器；没有适配器时返回明确错误。
pub async fn ble_central() -> Result<BleCentral, TransportError> {
    let manager = Manager::new().await.map_err(ble_err)?;
    let adapters = manager.adapters().await.map_err(ble_err)?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| TransportError::Ble("未检测到蓝牙适配器".into()))?;
    Ok(BleCentral {
        adapter,
        seen_order: std::sync::Mutex::new(Vec::new()),
    })
}

impl BleCentral {
    /// 开始扫描。不设系统级服务过滤：部分平台（Windows）的系统过滤会漏掉
    /// 服务 UUID 位于扫描响应包中的广播，导致永远扫不到；改为全量扫描，
    /// 由 list_found 按广播服务严格筛选（见下）。
    pub async fn start_scan(&self) -> Result<(), TransportError> {
        // SCANPROBE（仅日志）：新一轮扫描重置已 dump 集合并写节头
        if let Ok(mut g) = SCANPROBE_DUMPED.lock() {
            g.clear();
        }
        crate::probe::push_line(
            "SCANPROBE BLE广播 一节开始（本轮扫描，每台设备只记一次）".to_string(),
        );
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(ble_err)
    }

    /// 停止扫描。
    pub async fn stop_scan(&self) -> Result<(), TransportError> {
        self.adapter.stop_scan().await.map_err(ble_err)
    }

    /// 当前已发现的 BlueLink 设备列表（首次发现序，同名合并，见 merge_same_name）。
    pub async fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
        let mut out = Vec::new();
        for p in self.adapter.peripherals().await.map_err(ble_err)? {
            // 单台设备的属性读取失败只跳过它自己，不许连累整张列表
            let props = match p.properties().await {
                Ok(Some(props)) => props,
                Ok(None) => continue,
                Err(_) => continue,
            };
            dump_ble_device_once(&p.id().to_string(), &props);
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
                paired: false, // BLE 扫描结果不带配对信息
            });
        }
        // 稳定排序：按首次发现序排列（新发现的追加在后），信号只更新数值不改顺序
        {
            let mut order = self.seen_order.lock().unwrap();
            for d in &out {
                if !order.contains(&d.id) {
                    order.push(d.id.clone());
                }
            }
            // 随机地址会持续轮换，序表无界增长时丢最旧的一半（不影响已显示设备）
            if order.len() > 512 {
                order.drain(..256);
            }
            let idx = |id: &str| order.iter().position(|x| x == id).unwrap_or(usize::MAX);
            out.sort_by_key(|d| idx(&d.id));
        }
        Ok(merge_same_name(out))
    }

    /// 清掉一台设备的缓存状态（v2.6.1）：断开可能残留的僵尸 GATT 连接、
    /// 并把设备移出首次发现序。背景：对端进程重启后其广播地址/栈状态
    /// 已变，本机缓存里的旧 Peripheral 仍能在系统层「连上」，但对端应用
    /// 层毫无知觉（服务端零入站），握手必然超时——重试前必须先断掉这条
    /// 僵尸链路并配合重新扫描拿新地址。btleplug 没有缓存淘汰 API，
    /// disconnect + 忘掉排序记录 + 上层重启扫描是能做到的最彻底清理。
    pub async fn forget_device(&self, device_id: &str) {
        if let Ok(peripherals) = self.adapter.peripherals().await {
            for p in peripherals {
                if p.id().to_string() == device_id {
                    if matches!(p.is_connected().await, Ok(true)) {
                        let _ = p.disconnect().await;
                        crate::probe::log(
                            "ble",
                            &format!("已断开目标 {device_id} 的残留 GATT 连接（清缓存）"),
                        );
                    }
                    break;
                }
            }
        }
        if let Ok(mut order) = self.seen_order.lock() {
            order.retain(|x| x != device_id);
        }
    }

    /// 连接指定设备并完成 GATT 发现，返回可直接交给服务层握手的连接。
    ///
    /// 全程分步日志（步骤1–4/6，5–6 是服务层握手）：每步记完成耗时，
    /// 失败/超时记「卡在第几步、等了多久、底层错误原文」——真机连不上时
    /// 凭日志就能指认卡死点，不靠猜。各步共享 CONNECT_TIMEOUT 总截止：
    /// 对着已轮换走的旧广播地址或不应答的设备，底层 connect 可能长时间
    /// 不返回，超时直接判失败并给出人话原因，不许无限干等。
    pub async fn connect(&self, device_id: &str) -> Result<Conn, TransportError> {
        use std::time::Instant;
        let t0 = Instant::now();
        let total = || t0.elapsed().as_millis();
        let step_ms = |t: Instant| t.elapsed().as_millis();
        crate::probe::log_step("ble", &format!("连接步骤1/6 开始连接：目标 {device_id}"));
        // 目标不在当前扫描缓存里时不要立刻判死：断开后对端恢复广播、
        // 本机扫描重新发现都需要几秒，旧版即时报「不在扫描范围内」把
        // 刚断开后的立即重连误杀了（2026-11-20 真机实锤：断后 2 秒重连
        // 报此错、再点一次就成）。最长等 10 秒、每 500ms 复查缓存。
        let mut target: Option<CentralPeripheral> = None;
        let lookup_start = Instant::now();
        let mut wait_logged = false;
        loop {
            let peripherals = self.adapter.peripherals().await.map_err(ble_err)?;
            for p in peripherals {
                if p.id().to_string() == device_id {
                    target = Some(p);
                    break;
                }
            }
            if target.is_some() || lookup_start.elapsed() >= std::time::Duration::from_secs(10) {
                break;
            }
            if !wait_logged {
                wait_logged = true;
                crate::probe::log_step(
                    "ble",
                    "连接步骤1/6：目标暂不在扫描缓存内，等待其重新出现（最长 10 秒）",
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let peripheral = target.ok_or_else(|| {
            crate::probe::log(
                "ble",
                "连接失败于步骤1/6：设备已不在扫描范围内（等待 10 秒仍未出现）",
            );
            TransportError::Ble("设备已不在扫描范围内，请重新扫描".into())
        })?;
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;

        // 步骤2：无线电连接
        let t = Instant::now();
        if !peripheral.is_connected().await.map_err(ble_err)? {
            match tokio::time::timeout_at(deadline, peripheral.connect()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let e = ble_err(e);
                    crate::probe::log(
                        "ble",
                        &format!(
                            "连接失败于步骤2/6（无线电连接）：底层错误 {e}（本步耗时 {}ms）",
                            step_ms(t)
                        ),
                    );
                    return Err(e);
                }
                Err(_) => {
                    crate::probe::log(
                        "ble",
                        &format!(
                            "连接失败于步骤2/6（无线电连接）：超时，已等 {}ms 无响应",
                            step_ms(t)
                        ),
                    );
                    return Err(TransportError::Ble(
                        "BLE 连接超时：卡在步骤2/6（无线电连接），对方未响应，可能已不在附近或其广播地址已失效，请重新扫描后再试"
                            .into(),
                    ));
                }
            }
        }
        crate::probe::log_step(
            "ble",
            &format!(
                "连接步骤2/6 完成：无线电已连接（本步耗时 {}ms，总耗时 {}ms）",
                step_ms(t),
                total()
            ),
        );

        // 步骤3：服务发现（把找到的服务 UUID 全记下来，对端开没开 BlueLink 服务一看便知）
        let t = Instant::now();
        match tokio::time::timeout_at(deadline, peripheral.discover_services()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let e = ble_err(e);
                crate::probe::log(
                    "ble",
                    &format!(
                        "连接失败于步骤3/6（服务发现）：底层错误 {e}（本步耗时 {}ms）",
                        step_ms(t)
                    ),
                );
                return Err(e);
            }
            Err(_) => {
                crate::probe::log(
                    "ble",
                    &format!(
                        "连接失败于步骤3/6（服务发现）：超时，已等 {}ms 无响应",
                        step_ms(t)
                    ),
                );
                return Err(TransportError::Ble(
                    "BLE 连接超时：卡在步骤3/6（服务发现），对方未响应".into(),
                ));
            }
        }
        let svc_list: Vec<String> = peripheral
            .services()
            .iter()
            .map(|s| s.uuid.to_string())
            .collect();
        crate::probe::log_step(
            "ble",
            &format!(
                "连接步骤3/6 完成：发现 {} 个服务 [{}]（本步耗时 {}ms，总耗时 {}ms）",
                svc_list.len(),
                svc_list.join(", "),
                step_ms(t),
                total()
            ),
        );

        // 步骤4：特征值检查 + 订阅通知
        let t = Instant::now();
        let chars = peripheral.characteristics();
        let inbox = chars.iter().find(|c| c.uuid == INBOX_UUID).cloned();
        let outbox = chars.iter().find(|c| c.uuid == OUTBOX_UUID).cloned();
        crate::probe::log_step(
            "ble",
            &format!(
                "连接步骤4/6 特征值检查：写入特征{}、通知特征{}",
                if inbox.is_some() { "齐" } else { "缺" },
                if outbox.is_some() { "齐" } else { "缺" }
            ),
        );
        let inbox =
            inbox.ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 写入特征".into()))?;
        let outbox =
            outbox.ok_or_else(|| TransportError::Ble("对方缺少 BlueLink 通知特征".into()))?;
        match tokio::time::timeout_at(deadline, peripheral.subscribe(&outbox)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let e = ble_err(e);
                crate::probe::log(
                    "ble",
                    &format!(
                        "连接失败于步骤4/6（订阅通知）：底层错误 {e}（本步耗时 {}ms）",
                        step_ms(t)
                    ),
                );
                return Err(e);
            }
            Err(_) => {
                crate::probe::log(
                    "ble",
                    &format!(
                        "连接失败于步骤4/6（订阅通知）：超时，已等 {}ms 无响应",
                        step_ms(t)
                    ),
                );
                return Err(TransportError::Ble(
                    "BLE 连接超时：卡在步骤4/6（订阅通知），对方未响应".into(),
                ));
            }
        }
        crate::probe::log_step(
            "ble",
            &format!(
                "连接步骤4/6 完成：已订阅通知，链路就绪，等待握手（步骤5-6/6）（本步耗时 {}ms，总耗时 {}ms）",
                step_ms(t),
                total()
            ),
        );

        let (stream, ends) = channel_stream();
        let StreamEnds {
            incoming_tx,
            mut outgoing_rx,
        } = ends;

        // 入站泵：通知字节 → 切片序号验收 → 流。流结束必须落 essential——旧版静默退出，
        // 客户端对链路死亡毫无察觉（会话靠服务层心跳才判死，日志里还
        // 要有这条传输侧原因行才对得上账）
        let mut notifications = peripheral.notifications().await.map_err(ble_err)?;
        let in_tx = incoming_tx.clone();
        tokio::spawn(async move {
            let mut local_close = false;
            let mut slicer = SliceDecoder::new("ble");
            while let Some(n) = notifications.next().await {
                if n.uuid != OUTBOX_UUID {
                    continue;
                }
                let Some(bytes) = slicer.decode(&n.value) else {
                    continue; // 重复/损坏片已在解码器里计数落日志
                };
                if in_tx.send(bytes).await.is_err() {
                    local_close = true; // 本机流已关（会话收尾中），不是对端断链
                    break;
                }
            }
            if !local_close {
                crate::probe::log(
                    "ble",
                    "客户端：对端通知流结束（对端断开或链路丢失），会话将结束",
                );
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
            let mut slicer = SliceEncoder::new();
            while let Some((bytes, _slot)) = outgoing_rx.recv().await {
                let mut failed = false;
                for piece in slicer.encode(&bytes) {
                    if let Err(e) = peer.write(&inbox, &piece, write_type).await {
                        // 写失败是会话死因之一，落 essential 再拆链（旧版静默）
                        crate::probe::log(
                            "ble",
                            &format!("客户端：写失败（{}），主动断开链路", ble_err(e)),
                        );
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
    // 关键步骤先落日志再做事：进程若死在动作里，日志尾部即崩溃点（下同）
    crate::probe::log_step("ble", "服务端：正在发布服务（注册 BlueLink 服务与特征）");
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
    crate::probe::log_step(
        "ble",
        &format!(
            "服务端：服务发布成功（服务 {SERVICE_UUID}，写入特征 {INBOX_UUID}、通知特征 {OUTBOX_UUID}）"
        ),
    );
    crate::probe::log_step("ble", "服务端：正在启动广播");
    peripheral
        .start_advertising(local_name, &[SERVICE_UUID])
        .await
        .map_err(ble_err)?;
    crate::probe::log_step(
        "ble",
        &format!(
            "服务端：广播启动请求已接受（可连接、可发现）；广播内容：服务 {SERVICE_UUID}、本机名 {local_name}（广播包由系统按已发布服务自动生成）"
        ),
    );

    // 广播真状态验证（2026-11-19 真机定案后加）：库的 start_advertising 只
    // 发起请求，不等 Windows 把广播状态转到 Started；广播被系统中止时库也
    // 只写一行自己的 debug 日志、不通知应用——不验证的话状态行会「报喜不
    // 报忧」（显示已开启、空中其实没有广播）。这里轮询 is_advertising 确认
    // 真在广播（5 秒内每 500ms 一次），确认不了就如实报错，检查过程留日志。
    let mut advertising = false;
    for i in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match peripheral.is_advertising().await {
            Ok(true) => {
                advertising = true;
                crate::probe::log_step(
                    "ble",
                    &format!("服务端广播状态确认：已在广播（启动后第 {} 次检查）", i + 1),
                );
                break;
            }
            Ok(false) => {}
            Err(e) => {
                crate::probe::log("ble", &format!("服务端广播状态检查出错：{}", ble_err(e)));
                break;
            }
        }
    }
    if !advertising {
        crate::probe::log("ble", "服务端广播状态确认失败：启动后 5 秒仍未进入广播状态");
        return Err(TransportError::Ble(
            "广播启动后未能确认处于广播状态（系统可能拒绝了广播请求），对方将无法发现本机".into(),
        ));
    }

    let peripheral = Arc::new(Mutex::new(peripheral));
    let (inbound_tx, inbound_rx) = mpsc::channel::<Conn>(4);

    // 服务端事件循环：订阅建立链路，写入喂字节，取消订阅即断链
    let pump_peripheral = peripheral.clone();
    tokio::spawn(async move {
        let mut link: Option<(String, mpsc::Sender<Vec<u8>>, SliceDecoder)> = None;
        while let Some(event) = event_rx.recv().await {
            match event {
                PeripheralEvent::CharacteristicSubscriptionUpdate {
                    request,
                    subscribed,
                } if request.characteristic == OUTBOX_UUID => {
                    crate::probe::log_step(
                        "ble",
                        &format!(
                            "服务端：收到订阅变更（对方 {}，{}），开始处理",
                            request.client,
                            if subscribed { "订阅" } else { "取消订阅" }
                        ),
                    );
                    if subscribed {
                        let (stream, ends) = channel_stream();
                        let StreamEnds {
                            incoming_tx,
                            outgoing_rx: mut outgoing_rx,
                        } = ends;
                        link = Some((
                            request.client.clone(),
                            incoming_tx,
                            SliceDecoder::new("ble"),
                        ));
                        let out_peripheral = pump_peripheral.clone();
                        let out_client = request.client.clone();
                        tokio::spawn(async move {
                            let mut sent_once = false;
                            let mut slicer = SliceEncoder::new();
                            while let Some((bytes, _slot)) = outgoing_rx.recv().await {
                                let mut failed = false;
                                for piece in slicer.encode(&bytes) {
                                    // 锁只罩单片（旧版罩整帧：256KB 一帧上千片
                                    // 全程持锁，订阅事件/状态查询都被堵在后面）
                                    let mut p = out_peripheral.lock().await;
                                    // 通知发送加 5 秒超时与失败定案行（essential）：
                                    // 2026-11-20 真机实证——HelloAck 在这一段被
                                    // 静默吞掉（对端记「已回」、本机永收不到），
                                    // 旧代码 is_err 只 break 不记日志、挂死更是
                                    // 无声无息。从此失败/超时都在日志留痕。
                                    match tokio::time::timeout(
                                        std::time::Duration::from_secs(5),
                                        p.update_characteristic(OUTBOX_UUID, piece),
                                    )
                                    .await
                                    {
                                        Ok(Ok(())) => {}
                                        Ok(Err(e)) => {
                                            crate::probe::log(
                                                "ble",
                                                &format!(
                                                    "服务端：通知发送失败（对方 {out_client}）：{}，链路断开",
                                                    ble_err(e)
                                                ),
                                            );
                                            failed = true;
                                            break;
                                        }
                                        Err(_) => {
                                            crate::probe::log(
                                                "ble",
                                                &format!(
                                                    "服务端：通知发送超时（对方 {out_client}，5 秒未完成），链路断开"
                                                ),
                                            );
                                            failed = true;
                                            break;
                                        }
                                    }
                                }
                                if failed {
                                    break;
                                }
                                if !sent_once {
                                    sent_once = true;
                                    crate::probe::log_step(
                                        "ble",
                                        &format!(
                                            "服务端：首帧通知已发出（对方 {out_client}，{} 字节）",
                                            bytes.len()
                                        ),
                                    );
                                }
                            }
                        });
                        crate::probe::log_step(
                            "ble",
                            &format!(
                                "服务端：有设备连入并订阅通知（对方 {}），链路建立，等待握手",
                                request.client
                            ),
                        );
                        let conn = Conn::new(stream, format!("ble:{}", request.client));
                        if inbound_tx.send(conn).await.is_err() {
                            break;
                        }
                    } else if link
                        .as_ref()
                        .map(|(c, _, _)| c == &request.client)
                        .unwrap_or(false)
                    {
                        // 会话结束的原因行升 essential：普通模式下也要能查到
                        // 「对端退订」这个真实死因（旧版仅 verbose 有行）
                        crate::probe::log(
                            "ble",
                            &format!("服务端：对方取消订阅，链路断开（对方 {}）", request.client),
                        );
                        link = None; // 发送端随之丢弃，读端收到 EOF，服务层结束会话
                    }
                }
                PeripheralEvent::WriteRequest {
                    request,
                    value,
                    responder,
                    ..
                } => {
                    // 切片序号验收后喂流；通道满时**等**而不是丢（v2.6.2）：
                    // 旧版 try_send 满即丢片是字节流失步的实锤来源之一。
                    // 事件循环在此短暂等待只是把背压传给对端的写请求，
                    // 读端排空很快，不会久堵；通道已关（会话收尾）才放弃。
                    let mut deliver: Option<Vec<u8>> = None;
                    let mut target_tx: Option<mpsc::Sender<Vec<u8>>> = None;
                    if let Some((client, tx, slicer)) = link.as_mut() {
                        if request.characteristic == INBOX_UUID && client == &request.client {
                            deliver = slicer.decode(&value);
                            target_tx = Some(tx.clone());
                        }
                    }
                    if let (Some(bytes), Some(tx)) = (deliver, target_tx) {
                        if tx.send(bytes).await.is_err() {
                            crate::probe::log(
                                "ble",
                                "服务端：入站通道已关闭，丢弃一帧入站数据（会话收尾中）",
                            );
                        }
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
                PeripheralEvent::StateUpdate { is_powered } => {
                    if !is_powered {
                        // 无线电状态抖动不拆链（2026-11-20 双机日志实锤的
                        // 静默杀会话路径）：Radio.StateChanged 在扫描/查询
                        // 并发时可报瞬态非 On，旧版据此直接清链路——链路
                        // 其实没断、广播与事件循环都还活着，服务层却收到
                        // EOF 无声结束会话，对端 28 秒后才靠人工察觉。
                        // 从此只落 essential 观察行：真关电/真断链会经
                        // 通知失败、心跳判死等路径收尾，不靠这条事件。
                        crate::probe::log(
                            "ble",
                            "服务端：蓝牙无线电状态报告关闭（瞬态抖动不拆链，继续观察）",
                        );
                    } else {
                        crate::probe::log_step("ble", "服务端：蓝牙无线电状态开启");
                    }
                }
                _ => {}
            }
        }
        // 事件循环退出（外设事件通道关闭）必须留痕：旧版静默退出，
        // 服务端整体聋掉也无从排查
        crate::probe::log(
            "ble",
            "服务端：外设事件循环结束（事件通道关闭），服务端停止工作",
        );
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

    fn dev(id: &str, name: &str, rssi: Option<i16>) -> FoundDevice {
        FoundDevice {
            id: id.to_string(),
            name: name.to_string(),
            rssi,
            paired: false,
        }
    }

    /// 同名多身份合并：同台机器的轮换随机地址合成一条，代表取信号最强者，
    /// 位置留在首次出现处；未知设备不合并；不同名互不影响。
    #[test]
    fn 同名设备合并取最强信号() {
        let merged = merge_same_name(vec![
            dev("addr-old", "LAPTOP-A", Some(-92)), // 旧地址：信号停在弱值（0 格）
            dev("addr-x", "未知设备", Some(-50)),
            dev("addr-new", "LAPTOP-A", Some(-45)), // 当前地址：满格
            dev("addr-y", "未知设备", Some(-60)),
            dev("addr-b", "LAPTOP-B", Some(-70)),
        ]);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].name, "LAPTOP-A");
        assert_eq!(merged[0].id, "addr-new");
        assert_eq!(merged[0].rssi, Some(-45));
        assert_eq!(merged[1].name, "未知设备");
        assert_eq!(merged[2].name, "未知设备");
        assert_eq!(merged[3].name, "LAPTOP-B");
    }

    /// 合并时代表无信号而新身份有信号：新身份接替（不许把有信号的吞成无信号）。
    #[test]
    fn 同名合并无信号被有信号接替() {
        let merged = merge_same_name(vec![
            dev("a1", "LAPTOP-A", None),
            dev("a2", "LAPTOP-A", Some(-80)),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "a2");
        assert_eq!(merged[0].rssi, Some(-80));
    }

    /// 通道字节流经定长分片泵后仍能无损承载协议帧（含 300KB 大帧）。
    #[tokio::test]
    async fn 通道流分片重组无损() {
        let (stream_a, ends_a) = channel_stream();
        let (stream_b, ends_b) = channel_stream();
        // 交叉泵：A 出 → 64 字节分片 → B 入；B 出 → A 入
        let mut a_out = ends_a.outgoing_rx;
        let b_in = ends_b.incoming_tx.clone();
        tokio::spawn(async move {
            while let Some((bytes, _slot)) = a_out.recv().await {
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
            while let Some((bytes, _slot)) = b_out.recv().await {
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
            compression: bluelink_protocol::COMP_NONE,
            data: vec![9u8; 300 * 1024],
        });
        b.send(&big).await.unwrap();
        assert_eq!(a.recv().await.unwrap(), big);
    }

    /// 首发成功率回归（v2.6.2 验收线）：与生产泵同构的切片通道
    /// （SliceEncoder 逐片 → SliceDecoder 验收）连续传 200 帧大小
    /// 不一的文件片，逐帧核对完整到达——目标零首发失败。
    #[tokio::test]
    async fn 切片泵连续二百次传输零失败() {
        let (stream_a, ends_a) = channel_stream();
        let (stream_b, ends_b) = channel_stream();
        let mut a_out = ends_a.outgoing_rx;
        let b_in = ends_b.incoming_tx.clone();
        tokio::spawn(async move {
            let mut enc = SliceEncoder::new();
            let mut dec = SliceDecoder::new("test");
            while let Some((bytes, _slot)) = a_out.recv().await {
                for piece in enc.encode(&bytes) {
                    if let Some(frame_bytes) = dec.decode(&piece) {
                        if b_in.send(frame_bytes).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let a = Conn::new(stream_a, "ble:rate-a".into());
        let b = Conn::new(stream_b, "ble:rate-b".into());
        // 简易 LCG 伪随机填充，内容逐帧可复现校验
        let mut seed: u32 = 0x2545;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        for i in 0..200 {
            let size = 1024 + (i * 733 % 63_000);
            let data: Vec<u8> = (0..size).map(|_| rnd()).collect();
            let frame = Frame::Chunk(bluelink_protocol::FileChunk {
                transfer_id: "12345678-1234-1234-1234-1234567890ab".to_string(),
                offset: 0,
                compression: bluelink_protocol::COMP_NONE,
                data,
            });
            a.send(&frame).await.unwrap();
            let got = b.recv().await.unwrap();
            assert_eq!(got, frame, "第 {i} 帧内容不一致（首发失败）");
        }
    }

    /// 切片序号验收（v2.6.2）：重复片被丢弃计数、丢片被计数且不
    /// 连累后续片、正常流逐字节无损。
    #[test]
    fn 切片序号异常被检出() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let mut enc = SliceEncoder::new();
        let slices = enc.encode(&data);
        assert!(slices.len() > 10);
        // 正常流无损
        let mut dec = SliceDecoder::new("t");
        let mut out = Vec::new();
        for s in &slices {
            out.extend(dec.decode(s).unwrap());
        }
        assert_eq!(out, data);
        // 重复片：丢弃计数，内容仍无损
        let mut dec = SliceDecoder::new("t");
        let mut out = Vec::new();
        for (i, s) in slices.iter().enumerate() {
            if let Some(b) = dec.decode(s) {
                out.extend(b);
            }
            if i == 2 {
                assert!(dec.decode(s).is_none(), "重复片应被丢弃");
            }
        }
        assert_eq!(out, data);
        assert_eq!(dec.dups, 1);
        // 丢片：缺口计数，剩余载荷仍交付（受损帧由帧层 CRC 兜底）
        let mut dec = SliceDecoder::new("t");
        let mut got = 0usize;
        for (i, s) in slices.iter().enumerate() {
            if i == 1 {
                continue;
            }
            if let Some(b) = dec.decode(s) {
                got += b.len();
            }
        }
        assert_eq!(dec.gaps, 1);
        assert!(got < data.len());
    }
}
