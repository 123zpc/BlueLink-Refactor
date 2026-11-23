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
    use crate::ble::{
        channel_stream, merge_same_name, SliceDecoder, SliceEncoder, StreamEnds, CONNECT_TIMEOUT,
        INBOX_UUID, OUTBOX_UUID, SERVICE_UUID,
    };
    use crate::Conn;
    use futures_util::StreamExt;
    use simplersble::{
        Adapter, CharacteristicCapability, ClientEvent, ConnectionEvent, LocalCharacteristic,
        LocalPeripheral, Peripheral, ValueChangedEvent,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
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

    /// 一条链路的入站切片发送端（会话级）：多个生产者（通知流 / 写事件）共用，
    /// 断链时 take 掉并 drop，Conn 读端随即收到 EOF，与 ble 模块的语义一致。
    /// v2.6.2：入站先过切片序号验收（与 ble 模块同一套 SliceDecoder）；
    /// 喂流改 send().await 等待背压——旧版 try_send 满即丢片是字节流
    /// 失步的实锤来源之一，绝不许再静默丢字节。
    #[derive(Clone)]
    struct SessionTx {
        tx: Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>>,
        slicer: Arc<Mutex<SliceDecoder>>,
    }

    impl SessionTx {
        fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
            Self {
                tx: Arc::new(Mutex::new(Some(tx))),
                slicer: Arc::new(Mutex::new(SliceDecoder::new("simpleble"))),
            }
        }

        async fn send(&self, data: Vec<u8>) {
            // 重组解码器一次可带出 0..n 个按序载荷（乱序片入座等待时为 0，
            // 绝不按到达顺序喂流——v2.6.6 #19）
            let payloads = {
                let mut dec = self.slicer.lock().unwrap();
                dec.decode(&data)
            };
            if payloads.is_empty() {
                return;
            }
            let tx = self.tx.lock().unwrap().clone();
            if let Some(tx) = tx {
                for bytes in payloads {
                    if tx.send(bytes).await.is_err() {
                        break;
                    }
                }
            }
        }

        fn close(&self) {
            self.tx.lock().unwrap().take();
        }
    }

    // ================= 中心端（扫描 / 主动连接） =================

    /// 已发现设备表：句柄映射 + 首次发现序（列表按序稳定排列，
    /// 不许按 RSSI 实时重排——与 ble 模块同一口径）。
    struct FoundMap {
        order: Vec<String>,
        map: HashMap<String, Peripheral>,
    }

    /// SimpleBLE 中心端：持有一个系统蓝牙适配器与最近一次扫描命中的设备句柄。
    /// adapter 装进 Mutex 是为了能整体换新（见 reinit）：错误原文
    /// 「bad exception」意味着适配器/栈状态已损坏，继续用同一句柄重连
    /// 只会连环失败（2026-11-22 真机定案：自动重连头 4 次全撞此错、
    /// 每次白等 10 秒），必须先重初始化再连。
    pub struct SbleCentral {
        adapter: Arc<Mutex<Adapter>>,
        found: Arc<Mutex<FoundMap>>,
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
            adapter: Arc::new(Mutex::new(adapter)),
            found: Arc::new(Mutex::new(FoundMap {
                order: Vec::new(),
                map: HashMap::new(),
            })),
        })
    }

    impl SbleCentral {
        /// 当前适配器句柄的克隆（FFI 句柄本身是引用计数句柄，克隆廉价）。
        fn adapter(&self) -> Adapter {
            self.adapter.lock().unwrap().clone()
        }

        /// 适配器重初始化（v2.6.5，#18）：丢弃现有适配器句柄、重新枚举
        /// 取得新句柄，并清空发现表（旧句柄一并作废）。连接撞上
        /// 「bad exception」类错误后由应用层调用，再走重新扫描捕获。
        pub async fn reinit(&self) {
            let fresh = tokio::task::spawn_blocking(|| {
                Adapter::get_adapters().ok().and_then(|mut v| {
                    if v.is_empty() {
                        None
                    } else {
                        Some(v.remove(0))
                    }
                })
            })
            .await
            .ok()
            .flatten();
            match fresh {
                Some(a) => {
                    *self.adapter.lock().unwrap() = a;
                    let mut found = self.found.lock().unwrap();
                    found.order.clear();
                    found.map.clear();
                    crate::probe::log("simpleble", "适配器已重初始化（旧句柄与发现表已丢弃）");
                }
                None => {
                    crate::probe::log("simpleble", "适配器重初始化失败：未枚举到蓝牙适配器");
                }
            }
        }

        /// 开始扫描（SimpleBLE 扫描结果在 list_found 里按 BlueLink 服务 UUID 过滤）。
        pub async fn start_scan(&self) -> Result<(), TransportError> {
            let adapter = self.adapter();
            tokio::task::spawn_blocking(move || adapter.scan_start())
                .await
                .map_err(|e| TransportError::Ble(format!("扫描启动失败：{e}")))?
                .map_err(sble_err)
        }

        /// 停止扫描。
        pub async fn stop_scan(&self) -> Result<(), TransportError> {
            let adapter = self.adapter();
            tokio::task::spawn_blocking(move || adapter.scan_stop())
                .await
                .map_err(|e| TransportError::Ble(format!("扫描停止失败：{e}")))?
                .map_err(sble_err)
        }

        /// 当前已发现的 BlueLink 设备列表（首次发现序，同名合并）。
        /// 过滤口径与 ble 模块一致：只认广播服务里带 BlueLink 服务 UUID 的设备。
        pub async fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            let adapter = self.adapter();
            let svc = service_uuid();
            let hits = tokio::task::spawn_blocking(move || {
                let results = adapter.scan_get_results().map_err(sble_err)?;
                let mut hits: Vec<(FoundDevice, Peripheral)> = Vec::new();
                for p in results {
                    let is_bluelink = p
                        .services()
                        .map(|svcs| svcs.iter().any(|s| s.uuid().eq_ignore_ascii_case(&svc)))
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
                            paired: false, // SimpleBLE 扫描结果不带配对信息
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
                let mut found = self.found.lock().unwrap();
                for (dev, peripheral) in hits {
                    if !found.map.contains_key(&dev.id) {
                        found.order.push(dev.id.clone());
                    }
                    found.map.insert(dev.id.clone(), peripheral);
                    out.push(dev);
                }
                // 随机地址持续轮换，序表无界增长时丢最旧的一半
                if found.order.len() > 512 {
                    found.order.drain(..256);
                }
                let idx = |id: &str| {
                    found
                        .order
                        .iter()
                        .position(|x| x == id)
                        .unwrap_or(usize::MAX)
                };
                out.sort_by_key(|d| idx(&d.id));
            }
            // 同名合并：同一台机器的多个广播身份合成一条（见 ble::merge_same_name）
            Ok(merge_same_name(out))
        }

        /// 清掉一台设备的缓存状态（v2.6.1，与 ble 路同口径）：从发现表
        /// 移除其句柄并断开可能残留的连接。对端重启后旧句柄指向已失效的
        /// 广播地址/栈状态，不清掉重试只会再撞一次握手超时。
        pub async fn forget_device(&self, device_id: &str) {
            let peripheral = {
                let mut found = self.found.lock().unwrap();
                found.order.retain(|x| x != device_id);
                found.map.remove(device_id)
            };
            if let Some(p) = peripheral {
                // FFI 是阻塞调用，放阻塞线程执行；未连接时 disconnect
                // 返回错误，忽略即可——目的只是不留残留链路。
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = p.disconnect();
                })
                .await;
                crate::probe::log("simpleble", "已清掉目标的缓存句柄并断开残留连接");
            }
        }

        /// 连接指定设备并订阅 OUTBOX 通知，返回可直接交给服务层握手的连接。
        pub async fn connect(&self, device_id: &str) -> Result<Conn, TransportError> {
            // 连接分步日志（与 ble 路同一口径，步骤1–4/6，5–6 是服务层握手）
            let t0 = std::time::Instant::now();
            crate::probe::log_step(
                "simpleble",
                &format!("连接步骤1/6 开始连接：目标 {device_id}"),
            );
            // 目标不在扫描缓存时先等一等再判死（与 ble 路同口径）：断开后
            // 对端恢复广播、扫描重新发现都需要几秒，旧版即时报错把立即
            // 重连误杀了。最长等 10 秒、每 300ms 复查缓存。
            let lookup_start = std::time::Instant::now();
            let mut wait_logged = false;
            let peripheral = loop {
                let found = self.found.lock().unwrap().map.get(device_id).cloned();
                if let Some(p) = found {
                    break p;
                }
                if lookup_start.elapsed() >= std::time::Duration::from_secs(10) {
                    crate::probe::log(
                        "simpleble",
                        "连接失败于步骤1/6：设备已不在扫描范围内（等待 10 秒仍未出现）",
                    );
                    return Err(TransportError::Ble(
                        "设备已不在扫描范围内，请重新扫描".into(),
                    ));
                }
                if !wait_logged {
                    wait_logged = true;
                    crate::probe::log_step(
                        "simpleble",
                        "连接步骤1/6：目标暂不在扫描缓存内，等待其重新出现（最长 10 秒）",
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            };

            // 连接事件流要在 connect 之前订阅，否则断开事件可能漏掉。
            let mut conn_events = peripheral.on_connection_event();

            // 连接限时（CONNECT_TIMEOUT）：connect 是阻塞 FFI 掐不断，超时先
            // 判失败返回；阻塞线程若迟到成功，见作废标志立即自行断开，
            // 不留悬空链路（与 app 层取消令牌同一口径）。
            let abandoned = Arc::new(AtomicBool::new(false));
            let flag = abandoned.clone();
            let p_conn = peripheral.clone();
            let join = tokio::task::spawn_blocking(move || {
                crate::probe::log_step("simpleble", "连接步骤2/6 发起无线电连接（阻塞调用）");
                let tc = std::time::Instant::now();
                let r = p_conn.connect();
                // 步骤结果行归全量：失败情形紧接着另有「连接失败于步骤2/6」
                // 定案行走 essential，两种模式都不会漏掉失败。
                crate::probe::log_step(
                    "simpleble",
                    &format!(
                        "连接步骤2/6 无线电连接返回：{}（本步耗时 {}ms）",
                        if r.is_ok() { "成功" } else { "失败" },
                        tc.elapsed().as_millis()
                    ),
                );
                if flag.load(Ordering::SeqCst) {
                    if r.is_ok() {
                        let _ = p_conn.disconnect();
                    }
                    return Err(TransportError::Ble(
                        "连接已被放弃（超时迟到，已自动断开）".into(),
                    ));
                }
                r.map_err(sble_err)
            });
            match tokio::time::timeout(CONNECT_TIMEOUT, join).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    crate::probe::log(
                        "simpleble",
                        &format!("连接失败于步骤2/6（无线电连接）：底层错误 {e}"),
                    );
                    return Err(e);
                }
                Ok(Err(e)) => {
                    return Err(TransportError::Ble(format!("连接失败：{e}")));
                }
                Err(_) => {
                    abandoned.store(true, Ordering::SeqCst);
                    crate::probe::log(
                        "simpleble",
                        "连接失败于步骤2/6（无线电连接）：超时 20 秒，阻塞调用未返回",
                    );
                    return Err(TransportError::Ble(
                        "连接超时（20 秒）：卡在步骤2/6（无线电连接），对方未响应，可能已不在附近或其广播地址已失效，请重新扫描后再试"
                            .into(),
                    ));
                }
            }
            crate::probe::log_step(
                "simpleble",
                &format!(
                    "连接步骤2/6 完成：无线电已连接（总耗时 {}ms）",
                    t0.elapsed().as_millis()
                ),
            );

            // 服务发现：确认对方确实提供 BlueLink 服务与 INBOX/OUTBOX 特征。
            let svc = service_uuid();
            let inbox_s = inbox_uuid();
            let outbox_s = outbox_uuid();
            let services = peripheral.services().map_err(sble_err)?;
            let svc_list: Vec<String> = services.iter().map(|s| s.uuid()).collect();
            crate::probe::log_step(
                "simpleble",
                &format!(
                    "连接步骤3/6 完成：发现 {} 个服务 [{}]（总耗时 {}ms）",
                    svc_list.len(),
                    svc_list.join(", "),
                    t0.elapsed().as_millis()
                ),
            );
            let service = services
                .iter()
                .find(|s| s.uuid().eq_ignore_ascii_case(&svc))
                .ok_or_else(|| {
                    crate::probe::log(
                        "simpleble",
                        "连接失败于步骤3/6（服务发现）：对方没有 BlueLink 服务",
                    );
                    TransportError::Ble("对方缺少 BlueLink 服务".into())
                })?;
            let chars = service.characteristics();
            let inbox = chars
                .iter()
                .find(|c| c.uuid().eq_ignore_ascii_case(&inbox_s));
            let outbox = chars
                .iter()
                .find(|c| c.uuid().eq_ignore_ascii_case(&outbox_s));
            crate::probe::log_step(
                "simpleble",
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
            if !outbox.can_notify() {
                return Err(TransportError::Ble("对方通知特征不可订阅".into()));
            }
            // 无响应写吞吐更高；不支持时回落带响应写（与 ble 模块同一口径）。
            let write_command = inbox.can_write_command();
            if !write_command && !inbox.can_write_request() {
                return Err(TransportError::Ble("对方写入特征不可写".into()));
            }

            let mut values = peripheral.notify(&svc, &outbox_s).map_err(sble_err)?;
            crate::probe::log_step(
                "simpleble",
                &format!(
                    "连接步骤4/6 完成：已订阅通知，链路就绪，等待握手（步骤5-6/6）（总耗时 {}ms）",
                    t0.elapsed().as_millis()
                ),
            );

            let (stream, ends) = channel_stream();
            let StreamEnds {
                incoming_tx,
                outgoing_rx,
                aired_bytes,
            } = ends;
            let session = SessionTx::new(incoming_tx);

            // 入站泵：OUTBOX 通知字节 → 流
            let in_session = session.clone();
            tokio::spawn(async move {
                while let Some(ValueChangedEvent::ValueUpdated(data)) = values.next().await {
                    in_session.send(data).await;
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
            // 写看门狗（v2.6.5，#18）：FFI 写楔死时线程本身掐不断——转发
            // 任务兼任看门狗：队列里有货且写进度停滞超过 10 秒，判定无线
            // 电栈楔死，主动断开链路让服务层走故障断开 + 自动重连（重连
            // 会重新扫描捕获；bad exception 另有适配器重初始化兜底）。
            // 旧版无任何看门狗：剪贴板大图同步期间双向帧流冻结、双边
            // 只能靠心跳判死（真机实证 19 秒无入站、总中断约 90 秒）。
            let (std_tx, std_rx) = std::sync::mpsc::channel::<Vec<u8>>();
            let mut outgoing_rx = outgoing_rx;
            let now_ms = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            };
            let last_progress = Arc::new(std::sync::atomic::AtomicU64::new(now_ms()));
            let pending_msgs = Arc::new(std::sync::atomic::AtomicU64::new(0));
            {
                let p_disc = peripheral.clone();
                let w_session = session.clone();
                let last_progress_w = last_progress.clone();
                let pending_w = pending_msgs.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tokio::select! {
                            msg = outgoing_rx.recv() => {
                                match msg {
                                    Some((bytes, _slot)) => {
                                        pending_w.fetch_add(1, Ordering::Relaxed);
                                        // 入队也算活动（第三方评审采纳）：
                                        // 看门狗的 10 秒窗口从「最后一片写完
                                        // 与最后一条入队较晚者」起算，避免连接
                                        // 空闲久后第一条消息被误判楔死。
                                        last_progress_w.store(now_ms(), Ordering::Relaxed);
                                        if std_tx.send(bytes).is_err() {
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            _ = tick.tick() => {
                                let stalled = pending_w.load(Ordering::Relaxed) > 0
                                    && now_ms().saturating_sub(last_progress_w.load(Ordering::Relaxed)) > 10_000;
                                if stalled {
                                    crate::probe::log(
                                        "simpleble",
                                        "出站写进度停滞超过 10 秒（队列有货、帧流不动），判定无线电栈楔死，主动断开链路",
                                    );
                                    let p2 = p_disc.clone();
                                    let _ = tokio::task::spawn_blocking(move || {
                                        let _ = p2.disconnect();
                                    })
                                    .await;
                                    w_session.close();
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            let p_write = peripheral.clone();
            let w_session = session.clone();
            let aired_wr = aired_bytes.clone();
            std::thread::spawn(move || {
                let mut slicer = SliceEncoder::new();
                while let Ok(bytes) = std_rx.recv() {
                    let mut failed = false;
                    for data in slicer.encode(&bytes) {
                        let r = if write_command {
                            p_write.write_command(&svc, &inbox_s, &data)
                        } else {
                            p_write.write_request(&svc, &inbox_s, &data)
                        };
                        match r {
                            Ok(()) => {
                                aired_wr.fetch_add(
                                    data.len() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                last_progress.store(now_ms(), Ordering::Relaxed);
                            }
                            Err(_) => {
                                failed = true;
                                break;
                            }
                        }
                    }
                    pending_msgs.fetch_sub(1, Ordering::Relaxed);
                    if failed {
                        break;
                    }
                }
                let _ = p_write.disconnect();
                w_session.close();
            });

            Ok(Conn::new_with_air(
                stream,
                format!("simpleble:{device_id}"),
                aired_bytes,
            ))
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
            // 关键步骤先落日志再做事：进程若死在动作里，日志尾部即崩溃点
            crate::probe::log_step(
                "simpleble",
                "服务端：正在发布服务（注册 BlueLink 服务与特征）",
            );
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
                    &[
                        CharacteristicCapability::Notify,
                        CharacteristicCapability::Read,
                    ],
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
            crate::probe::log_step("simpleble", "服务端：正在启动广播");
            local.start().map_err(sble_err)?;
            crate::probe::log_step(
                "simpleble",
                &format!(
                    "服务端：服务发布与广播启动成功（广播带 BlueLink 服务 {}，可连接）",
                    service_uuid()
                ),
            );
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
                            crate::probe::log_step(
                                "simpleble",
                                &format!("服务端：有设备连入（对方 {addr}）"),
                            );
                            *client_addr.lock().unwrap() = Some(addr);
                        }
                        ClientEvent::Disconnected(addr) => {
                            crate::probe::log_step(
                                "simpleble",
                                &format!("服务端：对方断开（对方 {addr}）"),
                            );
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
                            crate::probe::log_step(
                                "simpleble",
                                "服务端：对方已订阅通知，链路建立，等待握手",
                            );
                            if link.lock().unwrap().is_some() {
                                continue; // 已有链路，忽略重复订阅
                            }
                            let (stream, ends) = channel_stream();
                            let StreamEnds {
                                incoming_tx,
                                mut outgoing_rx,
                                aired_bytes,
                            } = ends;
                            let session = SessionTx::new(incoming_tx);
                            *link.lock().unwrap() = Some(session.clone());
                            // 出站泵：流字节 → 按 ATT_CHUNK 切片 set_value 通知对端
                            let out_char = outbox_pump.clone();
                            let w_session = session.clone();
                            let aired_sv = aired_bytes.clone();
                            tokio::spawn(async move {
                                let mut slicer = SliceEncoder::new();
                                while let Some((bytes, _slot)) = outgoing_rx.recv().await {
                                    let mut failed = false;
                                    for piece in slicer.encode(&bytes) {
                                        match out_char.set_value(&piece) {
                                            Ok(()) => {
                                                aired_sv.fetch_add(
                                                    piece.len() as u64,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                            }
                                            Err(_) => {
                                                failed = true;
                                                break;
                                            }
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
                            let conn = Conn::new_with_air(
                                stream,
                                format!("simpleble:{addr}"),
                                aired_bytes,
                            );
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
                            session.send(data).await;
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
        TransportError::Ble("SimpleBLE 未编译进此版本（构建时未开启 simpleble feature）".into())
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

        pub async fn forget_device(&self, _device_id: &str) {}

        pub async fn reinit(&self) {}
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
