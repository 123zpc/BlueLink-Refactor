//! 经典蓝牙（BR/EDR + RFCOMM/SPP）传输（Windows 产品通道）。
//!
//! 配方逐项照老版本 BlueLink（Java + JNA 直调 Windows DLL，已在用户真机验证）：
//! - 发现：BluetoothFindFirstDevice 经典 Inquiry，搜索参数 fReturnAuthenticated /
//!   fReturnRemembered / fReturnUnknown / fReturnConnected 全开、fIssueInquiry = true，
//!   不按服务/UUID 过滤（老版本 BluetoothUtils.discoverDevices）。已配对/记住的设备
//!   即使当时不可被发现，也会出现在结果里。
//! - 服务端：Winsock socket(AF_BTH, SOCK_STREAM, BTHPROTO_RFCOMM) → bind 端口 7 →
//!   listen → WSASetService(RNRSERVICE_REGISTER) 把标准 SPP 服务
//!   （UUID 00001101-0000-1000-8000-00805F9B34FB，服务名 "BlueLink Server"）
//!   注册进系统 SDP 数据库（老版本 BluetoothServer）。
//! - 客户端：SOCKADDR_BTH 填对方 MAC + serviceClassId = SPP UUID + port = 0，
//!   connect() 时由系统经 SDP 自动解析对方 RFCOMM 通道（老版本 BluetoothClient）。
//! - 配对/加密由 Windows 蓝牙栈在系统层处理，应用不介入（与老版本一致）。
//!
//! 与 BLE 通道同构：已连接的 RFCOMM socket 经阻塞泵线程桥接到 ble::channel_stream，
//! 产出同一个 Conn，上层（服务层握手/聊天/文件/AI）完全不区分通道类型。
//! MAC、SDP、配对等经典蓝牙细节全部关在本模块内，不向上层泄漏：
//! 对外只暴露与 BleCentral/BleServer 对齐的方法与复用的 FoundDevice 结构。
//! 非 Windows 平台本模块为不支持空壳（classic_supported() 返回 false）。

use crate::ble::FoundDevice;
use crate::{Conn, TransportError};

/// 本平台是否支持经典蓝牙（仅 Windows）。
pub fn classic_supported() -> bool {
    cfg!(windows)
}

/// 经典设备 ID 前缀：扫描结果与 BLE 的平台 ID 区分，连接时据此解析 MAC。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
const ID_PREFIX: &str = "classic:";

/// MAC（u64，与 SOCKADDR_BTH.btAddr / BLUETOOTH_ADDRESS.ullLong 同布局）
/// 格式化为 AA:BB:CC:DD:EE:FF（老版本 BluetoothUtils.formatAddress）。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn mac_to_string(mac: u64) -> String {
    let mut s = String::with_capacity(17);
    for i in (0..6).rev() {
        if !s.is_empty() {
            s.push(':');
        }
        s.push_str(&format!("{:02X}", (mac >> (i * 8)) & 0xFF));
    }
    s
}

/// 解析 AA:BB:CC:DD:EE:FF 形式的 MAC 为 u64（与 mac_to_string 互逆）。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn parse_mac(text: &str) -> Option<u64> {
    let mut v: u64 = 0;
    let mut n = 0;
    for part in text.split(':') {
        v = (v << 8) | u64::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(v)
}

/// 扫描结果里经典设备的 ID（带前缀的 MAC 字符串）。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn device_id(mac: u64) -> String {
    format!("{ID_PREFIX}{}", mac_to_string(mac))
}

/// 从设备 ID 还原 MAC：接受带 classic: 前缀的扫描 ID，也接受裸 MAC 字符串。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn mac_from_device_id(id: &str) -> Option<u64> {
    parse_mac(id.strip_prefix(ID_PREFIX).unwrap_or(id))
}

/// AEP 信号的刻度护栏：≤ -25 视为绝对 dBm（与 BLE 的 RSSI 同刻度，UI 走现行
/// 格数阈值 -55/-65/-75/-85）；> -25（如已连接设备报出的 5）疑为连接态相对
/// 刻度（HCI Read RSSI 相对黄金接收范围的小数值），两种刻度未分清前不画格，
/// 只继续记 SCANPROBE 日志攒样本（用户 2026-11-19 真机日志定案后定的护栏）。
pub const AEP_RSSI_ABSOLUTE_MAX: i32 = -25;

/// 把 AEP 枚举到的 SignalStrength 按蓝牙地址并入经典设备列表：只补 rssi
/// 为空的条目，且只并入通过刻度护栏的绝对 dBm 值。AEP 只覆盖已关联/已配对
/// 设备，Inquiry 里的陌生设备查不到就保持无信号（不画格、不造数）。
/// lookup 按 MAC(u64) 查缓存（imp 的 AEP 枚举负责填充），抽出来便于单测。
pub fn merge_aep_rssi(devices: &mut [FoundDevice], lookup: &dyn Fn(u64) -> Option<i32>) {
    for d in devices.iter_mut() {
        if d.rssi.is_some() {
            continue;
        }
        let Some(mac) = mac_from_device_id(&d.id) else {
            continue;
        };
        if let Some(v) = lookup(mac) {
            if v <= AEP_RSSI_ABSOLUTE_MAX {
                d.rssi = Some(v as i16);
            }
        }
    }
}

/// Howard Hinnant 的 days_from_civil：公历日期 → 距 1970-01-01 的天数。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400; // [0, 399]
    let mp = i64::from((m + 9) % 12); // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 本地日期时间字段 → unix 风格秒。只用于与同为本地时的 stLastSeen 互相比较
/// （两侧同口径相减，时区不影响结论），不表示真实 UTC 时刻。
/// 年为 0（SYSTEMTIME 空值）或字段非法时返回 None。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn civil_to_unix_secs(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<i64> {
    if year <= 0 || !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let days = days_from_civil(i64::from(year), month, day);
    Some(days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))
}

/// 在场门容差（秒）：Inquiry 开始前刚被系统听到的设备，其 lastSeen 可能
/// 略早于本轮开始时刻，留 2 秒容差避免误杀真在场设备。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
const HEARD_TOLERANCE_SECS: i64 = 2;

/// 在场门（2026-11-20 v2.5.1）：本轮扫描是否真的「听到」了这台设备。
/// 经典 Inquiry 开了 fReturnRemembered，系统会把记住但不在场的设备也报出来
/// （用户实证：九个月前最后见到的设备冒充在场、还带着 AEP 缓存旧信号）。
/// 判定：当前已连接，或 lastSeen 落在本轮扫描窗口内（以轮始为界、带容差）。
#[cfg_attr(not(windows), allow(dead_code))] // 非 Windows 平台仅测试引用
fn heard_this_round(last_seen_secs: Option<i64>, connected: bool, round_start_secs: i64) -> bool {
    if connected {
        return true;
    }
    match last_seen_secs {
        Some(t) => t + HEARD_TOLERANCE_SECS >= round_start_secs,
        None => false,
    }
}

#[cfg(windows)]
mod imp {
    // Win32 FFI 结构体（SOCKADDR_BTH / WSAQUERYSETW 等）惯例是先全零再逐字段赋值，
    // 比结构体字面量更不易漏字段，故本模块豁免这条 clippy 规则。
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        civil_to_unix_secs, device_id, heard_this_round, mac_from_device_id, mac_to_string, Conn,
        FoundDevice, TransportError,
    };
    use crate::ble::{channel_stream, StreamEnds};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use windows_sys::core::GUID;
    use windows_sys::Win32::Devices::Bluetooth::{
        BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindFirstRadio,
        BluetoothFindNextDevice, BluetoothFindRadioClose, AF_BTH, BLUETOOTH_DEVICE_INFO,
        BLUETOOTH_DEVICE_SEARCH_PARAMS, BLUETOOTH_FIND_RADIO_PARAMS, BTHPROTO_RFCOMM,
        HBLUETOOTH_RADIO_FIND, NS_BTH, SOCKADDR_BTH,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Networking::WinSock::{
        accept, bind, closesocket, connect, getsockname, listen, recv, send, shutdown, socket,
        WSAGetLastError, WSASetServiceW, WSAStartup, CSADDR_INFO, INVALID_SOCKET,
        RNRSERVICE_DELETE, RNRSERVICE_REGISTER, SD_BOTH, SOCKADDR, SOCKET, SOCKET_ERROR,
        SOCK_STREAM, WSADATA, WSAESETSERVICEOP, WSAQUERYSETW,
    };
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    /// 标准 SPP 服务 UUID（00001101-0000-1000-8000-00805F9B34FB），与老版本一致：
    /// 服务端用它向 SDP 注册，客户端用它让系统解析 RFCOMM 通道。
    const SPP_GUID: GUID = GUID {
        data1: 0x0000_1101,
        data2: 0x0000,
        data3: 0x1000,
        data4: [0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB],
    };
    /// 服务端 RFCOMM 端口（老版本固定用 7；SDP 注册会把实际地址发布出去，
    /// 客户端经 SDP 解析通道，不依赖端口写死）。
    const SERVER_PORT: u32 = 7;
    /// Inquiry 超时倍数（单位 1.28 秒）：8 ≈ 10.2 秒一轮，与老版本 10 秒扫描相当。
    const INQUIRY_TIMEOUT_MULTIPLIER: u8 = 8;

    fn wsa_err(ctx: &str) -> TransportError {
        let code = unsafe { WSAGetLastError() };
        TransportError::Ble(format!("经典蓝牙{ctx}失败（WSA 错误 {code}）"))
    }

    /// 初始化 Winsock（2.2）。Winsock 按进程引用计数，这里每次调用都配平一次
    /// WSAStartup 且从不 WSACleanup（与老版本一致：生命周期由进程管理，
    /// 避免中途 Cleanup 让已建立的 socket 失效）。
    fn wsa_startup() -> Result<(), TransportError> {
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        let r = unsafe { WSAStartup(0x0202, &mut data) };
        if r != 0 {
            return Err(TransportError::Ble(format!(
                "经典蓝牙 WSAStartup 失败（错误 {r}）"
            )));
        }
        Ok(())
    }

    /// 取得本机第一个蓝牙适配器的查找句柄与 radio 句柄；无适配器时返回明确错误。
    ///
    /// # Safety
    /// 成功时返回的两个句柄归调用方所有，须以 close_radio 释放。
    unsafe fn first_radio() -> Result<(HBLUETOOTH_RADIO_FIND, HANDLE), TransportError> {
        let mut params = BLUETOOTH_FIND_RADIO_PARAMS::default();
        params.dwSize = std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32;
        let mut radio = std::ptr::null_mut();
        let find = BluetoothFindFirstRadio(&params, &mut radio);
        if find.is_null() {
            return Err(TransportError::Ble(
                "经典蓝牙未检测到蓝牙适配器".to_string(),
            ));
        }
        Ok((find, radio))
    }

    /// 释放 first_radio 取得的句柄对。
    ///
    /// # Safety
    /// 参数必须是 first_radio 返回且尚未释放的句柄。
    unsafe fn close_radio(find: HBLUETOOTH_RADIO_FIND, radio: HANDLE) {
        BluetoothFindRadioClose(find);
        CloseHandle(radio);
    }

    // ================= 字节流桥接 =================

    /// 把已连接的 RFCOMM socket 桥接成 Conn：两个阻塞线程做收发泵，
    /// 中间走 ble::channel_stream（与 BLE 通道完全同构）。
    /// socket 的关闭归写泵：发送通道关闭（Conn 被丢弃）或发送失败时关 socket，
    /// 读泵随之出错退出并给出 EOF，服务层据此结束会话。
    fn conn_from_socket(sock: SOCKET, peer: String) -> Conn {
        let (stream, ends) = channel_stream();
        let StreamEnds {
            incoming_tx,
            mut outgoing_rx,
        } = ends;
        // 读泵：socket → 通道
        {
            let tx = incoming_tx.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = unsafe { recv(sock, buf.as_mut_ptr(), buf.len() as i32, 0) };
                    if n <= 0 {
                        break; // 0 = 对端关闭，<0 = 出错或本端已关
                    }
                    if tx.blocking_send(buf[..n as usize].to_vec()).is_err() {
                        break;
                    }
                }
                // 本线程的 tx 随线程结束丢弃；conn_from_socket 里那份也已丢弃 → 读端 EOF
            });
        }
        // 写泵：通道 → socket
        std::thread::spawn(move || {
            while let Some(bytes) = outgoing_rx.blocking_recv() {
                let mut off = 0usize;
                let mut failed = false;
                while off < bytes.len() {
                    let n =
                        unsafe { send(sock, bytes[off..].as_ptr(), (bytes.len() - off) as i32, 0) };
                    if n <= 0 {
                        failed = true;
                        break;
                    }
                    off += n as usize;
                }
                if failed {
                    break;
                }
            }
            unsafe {
                shutdown(sock, SD_BOTH);
                closesocket(sock);
            }
        });
        Conn::new(stream, peer)
    }

    // ================= 中心端（Inquiry 扫描 / 主动连接） =================

    /// 经典蓝牙中心端：Inquiry 扫描 + 按 MAC 主动连接。
    pub struct ClassicCentral {
        scanning: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        found: Arc<Mutex<Vec<FoundDevice>>>,
    }

    /// 取得经典蓝牙中心端；无适配器时返回明确错误（与 ble_central 对齐）。
    pub fn classic_central() -> Result<ClassicCentral, TransportError> {
        wsa_startup()?;
        // 探一下适配器是否存在（只拿句柄随即释放）
        unsafe {
            let (find, radio) = first_radio()?;
            close_radio(find, radio);
        }
        Ok(ClassicCentral {
            scanning: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            found: Arc::new(Mutex::new(Vec::new())),
        })
    }

    impl ClassicCentral {
        /// 开始扫描：后台线程循环执行 Inquiry 并刷新快照（单轮约 10 秒，
        /// 界面侧每秒调 list_found 读最新一轮结果，与 BLE 轮询同构）。
        pub fn start_scan(&self) -> Result<(), TransportError> {
            if self.scanning.swap(true, Ordering::SeqCst) {
                return Ok(()); // 已在扫描
            }
            let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            let scanning = self.scanning.clone();
            let generation = self.generation.clone();
            let found = self.found.clone();
            std::thread::spawn(move || {
                // SCANPROBE（仅日志）：每次扫描只 dump 第一轮 Inquiry 的原始全字段
                let mut dumped = false;
                // 首次发现序：跨轮保持列表顺序稳定，新设备追加在后，
                // 不许每轮按 Inquiry 返回序重排导致列表跳动（与 BLE 同口径）
                let mut order: Vec<String> = Vec::new();
                while scanning.load(Ordering::SeqCst) && generation.load(Ordering::SeqCst) == gen {
                    // 单轮失败（适配器忙等）不终止扫描，下一轮重试——与 BLE 轮询容错一致
                    if let Ok((mut devs, dump)) = run_inquiry() {
                        if !dumped {
                            dumped = true;
                            dump.into_iter().for_each(crate::probe::push_line);
                        }
                        for d in &devs {
                            if !order.contains(&d.id) {
                                order.push(d.id.clone());
                            }
                        }
                        let idx =
                            |id: &str| order.iter().position(|x| x == id).unwrap_or(usize::MAX);
                        devs.sort_by_key(|d| idx(&d.id));
                        if let Ok(mut slot) = found.lock() {
                            *slot = devs;
                        }
                    }
                }
            });
            Ok(())
        }

        /// 停止扫描（当前轮 Inquiry 跑完后线程退出）。
        pub fn stop_scan(&self) -> Result<(), TransportError> {
            self.scanning.store(false, Ordering::SeqCst);
            Ok(())
        }

        /// 最新一轮 Inquiry 的设备快照：已在 inquiry 建表处过在场门（只含本轮
        /// 真听到或已连接的设备，见 heard_this_round）；是不是 BlueLink 由连接
        /// 后的服务层握手判定。返回前把 AEP 枚举缓存里的信号按地址并入——
        /// 只作用于在场设备（信号门），不在场设备不可能带格出现。
        pub fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            let mut devs = self.found.lock().map(|f| f.clone()).unwrap_or_default();
            super::merge_aep_rssi(&mut devs, &|mac| {
                AEP_RSSI_CACHE
                    .lock()
                    .ok()
                    .and_then(|c| c.get(&mac).copied())
            });
            Ok(devs)
        }

        /// 连接指定设备：SOCKADDR_BTH 填 MAC + SPP UUID + port = 0，
        /// connect 时由系统经 SDP 解析对方 RFCOMM 通道（老版本客户端配方）。
        /// 连接成功后产出 Conn，之后走服务层连接码握手。
        pub async fn connect(&self, device_id: &str) -> Result<Conn, TransportError> {
            let mac = mac_from_device_id(device_id)
                .ok_or_else(|| TransportError::Ble("经典蓝牙设备地址无效".to_string()))?;
            tokio::task::spawn_blocking(move || connect_blocking(mac))
                .await
                .map_err(|e| TransportError::Ble(format!("经典蓝牙连接任务异常：{e}")))?
        }
    }

    fn connect_blocking(mac: u64) -> Result<Conn, TransportError> {
        wsa_startup()?;
        unsafe {
            let sock = socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32);
            if sock == INVALID_SOCKET {
                return Err(wsa_err("创建 socket"));
            }
            let mut sa = SOCKADDR_BTH::default();
            sa.addressFamily = AF_BTH;
            sa.btAddr = mac;
            sa.serviceClassId = SPP_GUID;
            sa.port = 0; // port = 0 + 服务 UUID：系统经 SDP 解析对方通道（老版本配方）
            let r = connect(
                sock,
                std::ptr::addr_of!(sa) as *const SOCKADDR,
                std::mem::size_of::<SOCKADDR_BTH>() as i32,
            );
            if r == SOCKET_ERROR {
                let e = wsa_err("连接");
                closesocket(sock);
                return Err(e);
            }
            Ok(conn_from_socket(
                sock,
                format!("classic:{}", mac_to_string(mac)),
            ))
        }
    }

    /// 执行一轮经典 Inquiry（阻塞，约 10 秒），返回本轮发现的全部设备。
    /// 参数照老版本：已认证/已记住/未知/已连接全返回 + 发起 Inquiry，不按服务过滤。
    fn run_inquiry() -> Result<(Vec<FoundDevice>, Vec<String>), TransportError> {
        unsafe {
            let (find_radio, radio) = first_radio()?;
            let result = inquiry_on_radio(radio);
            close_radio(find_radio, radio);
            result
        }
    }

    /// # Safety
    /// radio 必须是有效的蓝牙适配器句柄。
    unsafe fn inquiry_on_radio(
        radio: HANDLE,
    ) -> Result<(Vec<FoundDevice>, Vec<String>), TransportError> {
        // 本轮开始时刻（本地时，与 stLastSeen 同口径换算成 unix 风格秒）：
        // 在场门的判定界。取不到时用 0——此时 lastSeen 判定会全部放行，
        // 退回旧行为而不是误杀全部设备。
        let mut now_st = windows_sys::Win32::Foundation::SYSTEMTIME::default();
        GetLocalTime(&mut now_st);
        let round_start = civil_to_unix_secs(
            i32::from(now_st.wYear),
            u32::from(now_st.wMonth),
            u32::from(now_st.wDay),
            u32::from(now_st.wHour),
            u32::from(now_st.wMinute),
            u32::from(now_st.wSecond),
        )
        .unwrap_or(0);
        let mut params: BLUETOOTH_DEVICE_SEARCH_PARAMS = std::mem::zeroed();
        params.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32;
        params.fReturnAuthenticated = 1;
        params.fReturnRemembered = 1;
        params.fReturnUnknown = 1;
        params.fReturnConnected = 1;
        params.fIssueInquiry = 1; // 执行经典 Inquiry（与 BLE 广播扫描是两套独立过程）
        params.cTimeoutMultiplier = INQUIRY_TIMEOUT_MULTIPLIER;
        params.hRadio = radio;

        let mut info = BLUETOOTH_DEVICE_INFO::default();
        info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
        let find = BluetoothFindFirstDevice(&params, &mut info);
        if find.is_null() {
            // 一台设备都没有时返回 NULL，不算故障
            return Ok((
                Vec::new(),
                vec![
                    "SCANPROBE 经典Inquiry 一节开始：本轮 0 台（原始结构全字段）".to_string(),
                    "SCANPROBE 经典Inquiry 一节结束".to_string(),
                ],
            ));
        }
        let mut out: Vec<FoundDevice> = Vec::new();
        let mut raws: Vec<BLUETOOTH_DEVICE_INFO> = Vec::new();
        let mut filtered_out = 0usize; // 在场门滤掉的「记住但本轮未应答」台数
        loop {
            let mac = info.Address.Anonymous.ullLong;
            let id = device_id(mac);
            if !raws.iter().any(|r| r.Address.Anonymous.ullLong == mac) {
                raws.push(info);
            }
            // 在场门：记住但本轮没应答的设备不进列表（SCANPROBE dump 仍保留
            // 全量原始数据，仅日志）。信号门随之成立：list_found 的 AEP 并入
            // 只作用于出表后的在场设备，不在场者不可能带格出现。
            let last_seen = civil_to_unix_secs(
                i32::from(info.stLastSeen.wYear),
                u32::from(info.stLastSeen.wMonth),
                u32::from(info.stLastSeen.wDay),
                u32::from(info.stLastSeen.wHour),
                u32::from(info.stLastSeen.wMinute),
                u32::from(info.stLastSeen.wSecond),
            );
            if !heard_this_round(last_seen, info.fConnected != 0, round_start) {
                filtered_out += 1;
                info = BLUETOOTH_DEVICE_INFO::default();
                info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
                if BluetoothFindNextDevice(find, &mut info) == 0 {
                    break;
                }
                continue;
            }
            if !out.iter().any(|d: &FoundDevice| d.id == id) {
                let end = info
                    .szName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szName.len());
                let name = String::from_utf16_lossy(&info.szName[..end]);
                out.push(FoundDevice {
                    id,
                    name: if name.trim().is_empty() {
                        "未知设备".to_string()
                    } else {
                        name
                    },
                    rssi: None, // 经典 Inquiry 不提供信号强度
                    // 已配对（fAuthenticated）或系统已记住（fRemembered）的设备
                    paired: info.fRemembered != 0 || info.fAuthenticated != 0,
                });
            }
            info = BLUETOOTH_DEVICE_INFO::default();
            info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
            if BluetoothFindNextDevice(find, &mut info) == 0 {
                break;
            }
        }
        BluetoothFindDeviceClose(find);
        // SCANPROBE（仅日志）：本轮每台设备的原始结构逐字段分段
        let mut dump = vec![format!(
            "SCANPROBE 经典Inquiry 一节开始：本轮 {} 台（原始结构全字段）",
            raws.len()
        )];
        for raw in &raws {
            dump.extend(dump_inquiry_device(raw));
        }
        dump.push(format!(
            "SCANPROBE 在场门：本轮原始 {} 台，滤掉记住未应答 {} 台，列表保留 {} 台",
            raws.len(),
            filtered_out,
            out.len()
        ));
        dump.push("SCANPROBE 经典Inquiry 一节结束".to_string());
        Ok((out, dump))
    }

    /// 把一台 Inquiry 设备的 BLUETOOTH_DEVICE_INFO 全字段 dump 成 SCANPROBE 分段行。
    fn dump_inquiry_device(info: &BLUETOOTH_DEVICE_INFO) -> Vec<String> {
        // Address 是联合体，读 ullLong 需 unsafe（调用方保证结构有效）
        let mac = mac_to_string(unsafe { info.Address.Anonymous.ullLong });
        let end = info
            .szName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szName.len());
        let name = String::from_utf16_lossy(&info.szName[..end]);
        let cod = info.ulClassofDevice;
        let systime = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| -> String {
            if t.wYear == 0 {
                "（空）".to_string()
            } else {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
                )
            }
        };
        vec![
            format!("SCANPROBE 设备开始 [classic-inquiry] {mac}"),
            format!("SCANPROBE 地址={mac}"),
            format!(
                "SCANPROBE 名称={}",
                if name.trim().is_empty() {
                    "（空）".to_string()
                } else {
                    name
                }
            ),
            format!("SCANPROBE CoD原始=0x{cod:06X}"),
            format!("SCANPROBE CoD大类={}", (cod >> 8) & 0x1F),
            format!("SCANPROBE CoD小类={}", (cod >> 2) & 0x3F),
            format!("SCANPROBE CoD服务位=0x{:03X}", (cod >> 13) & 0x7FF),
            format!("SCANPROBE 已认证={}", info.fAuthenticated != 0),
            format!("SCANPROBE 已记住={}", info.fRemembered != 0),
            format!("SCANPROBE 已连接={}", info.fConnected != 0),
            format!("SCANPROBE 最近见过={}", systime(&info.stLastSeen)),
            format!("SCANPROBE 最近使用={}", systime(&info.stLastUsed)),
            format!("SCANPROBE 设备结束 [classic-inquiry] {mac}"),
        ]
    }

    // ================= 服务端（RFCOMM 监听 / SDP 注册） =================

    struct ServerShared {
        listen_sock: Mutex<Option<SOCKET>>,
        /// getsockname 取回的实际绑定地址，注销 SDP 时重建查询集用。
        local_sa: SOCKADDR_BTH,
        registered: AtomicBool,
    }

    /// 经典蓝牙服务端句柄：RFCOMM 监听中，SPP 服务已（尝试）注册到系统 SDP。
    pub struct ClassicServer {
        shared: Arc<ServerShared>,
    }

    impl ClassicServer {
        /// 本机 SPP 服务是否已成功注册到系统 SDP。
        /// 注册失败时对方按 SPP UUID 找不到本机（老版本同样在注册失败时保留监听）。
        pub fn is_registered(&self) -> bool {
            self.shared.registered.load(Ordering::SeqCst)
        }

        /// 停止服务：注销 SDP 记录并关闭监听 socket（accept 线程随之退出）。
        pub fn stop(&self) {
            if self.shared.registered.swap(false, Ordering::SeqCst) {
                let _ = set_service(&self.shared.local_sa, RNRSERVICE_DELETE);
            }
            if let Ok(mut slot) = self.shared.listen_sock.lock() {
                if let Some(sock) = slot.take() {
                    unsafe {
                        shutdown(sock, SD_BOTH);
                        closesocket(sock);
                    }
                }
            }
        }
    }

    /// 启动经典蓝牙服务端：同步完成 bind / listen / SDP 注册，失败如实返回错误。
    /// 与 BLE 服务端并行存在，哪个通道连进来就走哪个，服务层不区分。
    /// 返回句柄与入站连接接收端：每个连入的对端产生一个 Conn。
    pub fn start_classic_server() -> Result<(ClassicServer, mpsc::Receiver<Conn>), TransportError> {
        wsa_startup()?;
        unsafe {
            let sock = socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32);
            if sock == INVALID_SOCKET {
                return Err(wsa_err("创建服务端 socket"));
            }
            let bound = match bind_listen_getname(sock) {
                Ok(b) => b,
                Err(e) => {
                    closesocket(sock);
                    return Err(e);
                }
            };
            // 向系统 SDP 注册标准 SPP 服务，客户端才能按 UUID 找到本机（老版本关键一步）。
            // 注册失败不致命：状态经 ClassicServer::is_registered 如实上报，监听继续。
            let registered = set_service(&bound, RNRSERVICE_REGISTER).is_ok();
            let shared = Arc::new(ServerShared {
                listen_sock: Mutex::new(Some(sock)),
                local_sa: bound,
                registered: AtomicBool::new(registered),
            });
            let (inbound_tx, inbound_rx) = mpsc::channel::<Conn>(4);
            let accept_shared = shared.clone();
            std::thread::spawn(move || accept_loop(accept_shared, inbound_tx));
            Ok((ClassicServer { shared }, inbound_rx))
        }
    }

    /// bind（端口 7）→ listen → getsockname 取回实际绑定地址（老版本同序）。
    ///
    /// # Safety
    /// sock 必须是刚创建的 AF_BTH socket。
    unsafe fn bind_listen_getname(sock: SOCKET) -> Result<SOCKADDR_BTH, TransportError> {
        let mut sa = SOCKADDR_BTH::default();
        sa.addressFamily = AF_BTH;
        sa.port = SERVER_PORT; // btAddr = 0：任意本地适配器，与老版本一致
        if bind(
            sock,
            std::ptr::addr_of!(sa) as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_BTH>() as i32,
        ) == SOCKET_ERROR
        {
            return Err(wsa_err("绑定端口"));
        }
        if listen(sock, 5) == SOCKET_ERROR {
            return Err(wsa_err("监听"));
        }
        let mut bound = SOCKADDR_BTH::default();
        let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
        if getsockname(
            sock,
            std::ptr::addr_of_mut!(bound) as *mut SOCKADDR,
            &mut len,
        ) == SOCKET_ERROR
        {
            return Err(wsa_err("获取本地地址"));
        }
        Ok(bound)
    }

    fn accept_loop(shared: Arc<ServerShared>, inbound_tx: mpsc::Sender<Conn>) {
        loop {
            let listen_sock = match shared.listen_sock.lock() {
                Ok(slot) => match *slot {
                    Some(s) => s,
                    None => break, // stop() 已关
                },
                Err(_) => break,
            };
            let mut peer_sa = SOCKADDR_BTH::default();
            let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
            let client = unsafe {
                accept(
                    listen_sock,
                    std::ptr::addr_of_mut!(peer_sa) as *mut SOCKADDR,
                    &mut len,
                )
            };
            if client == INVALID_SOCKET {
                // stop() 关监听 socket 时 accept 在此失败退出；其余错误同样结束本轮服务
                break;
            }
            let conn =
                conn_from_socket(client, format!("classic:{}", mac_to_string(peer_sa.btAddr)));
            if inbound_tx.blocking_send(conn).is_err() {
                break; // 应用已退出
            }
        }
    }

    /// 向系统 SDP 注册/注销 SPP 服务（WSAQUERYSET 组法照老版本 BluetoothServer：
    /// 服务名 BlueLink Server、CSADDR 的 Local/Remote 都填 getsockname 的绑定地址）。
    fn set_service(local_sa: &SOCKADDR_BTH, op: WSAESETSERVICEOP) -> Result<(), TransportError> {
        unsafe {
            let mut name: Vec<u16> = "BlueLink Server"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut comment: Vec<u16> = "BlueLink Bluetooth Service"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut guid = SPP_GUID;
            let sa_ptr = local_sa as *const SOCKADDR_BTH as *mut SOCKADDR;
            let mut csa = CSADDR_INFO::default();
            csa.iSocketType = SOCK_STREAM;
            csa.iProtocol = BTHPROTO_RFCOMM as i32;
            csa.LocalAddr.lpSockaddr = sa_ptr;
            csa.LocalAddr.iSockaddrLength = std::mem::size_of::<SOCKADDR_BTH>() as i32;
            csa.RemoteAddr.lpSockaddr = sa_ptr;
            csa.RemoteAddr.iSockaddrLength = std::mem::size_of::<SOCKADDR_BTH>() as i32;

            let mut qs = WSAQUERYSETW::default();
            qs.dwSize = std::mem::size_of::<WSAQUERYSETW>() as u32;
            qs.lpszServiceInstanceName = name.as_mut_ptr();
            qs.lpszComment = comment.as_mut_ptr();
            qs.lpServiceClassId = &mut guid;
            qs.dwNameSpace = NS_BTH;
            qs.dwNumberOfCsAddrs = 1;
            qs.lpcsaBuffer = &mut csa;
            if WSASetServiceW(&qs, op, 0) == SOCKET_ERROR {
                return Err(wsa_err(if op == RNRSERVICE_REGISTER {
                    "注册 SDP 服务"
                } else {
                    "注销 SDP 服务"
                }));
            }
            Ok(())
        }
    }

    // ---------- SCANPROBE：AEP 属性探针 + 经典信号数据通路 ----------
    // 背景：经典 Inquiry 的结果结构 BLUETOOTH_DEVICE_INFO 没有 RSSI 字段，
    // 驱动层 BTH_DEVICE_INFO / BTH_RADIO_IN_RANGE 同样没有，WinRT 经典
    // BluetoothDevice 类也没有信号属性。唯一出路是 AEP 属性
    // System.Devices.Aep.SignalStrength——2026-11-19 用户真机日志已定案：
    // 已配对电脑确实带值（未连接时 -42 为标准 dBm 刻度；已连接时报 5，
    // 疑为连接态相对刻度）。自 v2.5.0 起本节从「仅日志探针」转正为数据
    // 通路：枚举时把 (DeviceAddress, SignalStrength) 原始值写入
    // AEP_RSSI_CACHE，list_found 按地址并入（刻度护栏在 merge_aep_rssi）。
    // SCANPROBE 全字段 dump 保留：>-25 的相对刻度样本靠它继续积累，
    // 也便于日后复查 AEP 还给了什么。60 秒节流沿用，不拖慢扫描出表。
    // 属性申请走防御式（choose_aep_props + 枚举降级链）：v2.4.0 首测时整组
    // 申请被一个不存在的键 ModelNumber 拖垮（0x8002802B），本轮起坏键只会被
    // 逐个剔除并记日志，AEP 一节必有产出。

    static AEP_PROBE_RUNNING: AtomicBool = AtomicBool::new(false);
    static AEP_PROBE_LAST_RUN: AtomicU64 = AtomicU64::new(0);

    /// AEP 信号缓存：MAC(u64) → SignalStrength 原始值（未过刻度护栏的原值，
    /// 护栏在合并时应用）。由 AEP 枚举（60 秒节流）刷新，list_found 读取。
    static AEP_RSSI_CACHE: std::sync::LazyLock<Mutex<std::collections::HashMap<u64, i32>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

    /// AEP 核心键：逐项与微软官方《Device information properties》的
    /// AssociationEndpoint 属性表核对无误；探针的重点 SignalStrength 在此组，
    /// 降级重试时以此组保底。
    const AEP_PROPS_CORE: &[&str] = &[
        "System.Devices.Aep.SignalStrength",
        "System.Devices.Aep.IsPaired",
        "System.Devices.Aep.CanPair",
        "System.Devices.Aep.IsConnected",
        "System.Devices.Aep.IsPresent",
        "System.Devices.Aep.DeviceAddress",
    ];

    /// AEP 扩展键：同样逐项核对官方属性表，与核心键分开申请，单个坏键只会被
    /// 剔除、不会连累整组枚举。
    /// 注：v2.4.0 及之前曾申请 `System.Devices.Aep.ModelNumber`——官方属性表里
    /// 根本没有这个键（型号只有 ModelId（GUID）/ ModelName（String）两个键），
    /// 它就是让整组枚举报「Property key syntax error」(0x8002802B)、AEP 一节
    /// 一条没产出的坏键（用户 2026-11-19 真机日志实证），本轮已剔除并改用
    /// ModelId / ModelName。
    const AEP_PROPS_EXT: &[&str] = &[
        "System.Devices.Aep.AepId",
        "System.Devices.Aep.Category",
        "System.Devices.Aep.ContainerId",
        "System.Devices.Aep.Manufacturer",
        "System.Devices.Aep.ModelId",
        "System.Devices.Aep.ModelName",
        "System.Devices.Aep.ProtocolId",
        "System.ItemNameDisplay",
    ];

    /// 启动一次 AEP 探针（后台线程；60 秒内重复调用直接跳过）。
    pub fn spawn_aep_probe() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if AEP_PROBE_RUNNING.swap(true, Ordering::SeqCst) {
            return;
        }
        if now.saturating_sub(AEP_PROBE_LAST_RUN.load(Ordering::SeqCst)) < 60 {
            AEP_PROBE_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
        AEP_PROBE_LAST_RUN.store(now, Ordering::SeqCst);
        std::thread::spawn(move || {
            match run_aep_probe() {
                Ok(lines) => lines.into_iter().for_each(crate::probe::push_line),
                Err(e) => crate::probe::push_line(format!("SCANPROBE WinRT-AEP 枚举失败：{e}")),
            }
            AEP_PROBE_RUNNING.store(false, Ordering::SeqCst);
        });
    }

    /// 试建一个带给定附加属性的 DeviceWatcher（不启动）：创建期即解析属性键，
    /// 键不合法时在此报错——用来逐键/逐组校验，避免坏键连累整组枚举。
    fn aep_watcher_creates(selector: &windows::core::HSTRING, keys: &[&str]) -> Result<(), String> {
        use windows::core::HSTRING;
        use windows::Devices::Enumeration::DeviceInformation;
        let props: windows_collections::IIterable<HSTRING> = keys
            .iter()
            .map(|s| HSTRING::from(*s))
            .collect::<Vec<_>>()
            .into();
        DeviceInformation::CreateWatcherAqsFilterAndAdditionalProperties(selector, &props)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 防御式选定本轮申请的属性集：先整组（核心+扩展）试建；不行则核心组打底、
    /// 扩展键逐个增补校验；再不行则全部键逐个校验。被判无效的键逐个记一行
    /// 「SCANPROBE AEP 属性键无效：<键>」，保证坏键永远只影响它自己。
    /// 返回 (最终属性集, 说明行)。
    fn choose_aep_props(selector: &windows::core::HSTRING) -> (Vec<&'static str>, Vec<String>) {
        let mut notes = Vec::new();
        let all: Vec<&'static str> = AEP_PROPS_CORE
            .iter()
            .chain(AEP_PROPS_EXT.iter())
            .copied()
            .collect();
        // 第一级：整组试建，成功就整组用
        if aep_watcher_creates(selector, &all).is_ok() {
            return (all, notes);
        }
        // 第二级：核心组打底，扩展键逐个增补
        if aep_watcher_creates(selector, AEP_PROPS_CORE).is_ok() {
            let mut accepted: Vec<&'static str> = AEP_PROPS_CORE.to_vec();
            for key in AEP_PROPS_EXT {
                let mut trial = accepted.clone();
                trial.push(key);
                if aep_watcher_creates(selector, &trial).is_ok() {
                    accepted.push(key);
                } else {
                    notes.push(format!("SCANPROBE AEP 属性键无效：{key}"));
                }
            }
            return (accepted, notes);
        }
        // 第三级：核心组整组都建不出来——全部键逐个校验，只留能用的
        let mut accepted: Vec<&'static str> = Vec::new();
        for key in all {
            if aep_watcher_creates(selector, &[key]).is_ok() {
                accepted.push(key);
            } else {
                notes.push(format!("SCANPROBE AEP 属性键无效：{key}"));
            }
        }
        (accepted, notes)
    }

    /// 探针主流程：选定属性集 → 枚举；枚举失败则逐级降级（已选集 → 核心键组
    /// → 空属性集），保证 AEP 一节必有产出；只有连裸枚举都失败才整体报错。
    fn run_aep_probe() -> Result<Vec<String>, String> {
        use windows::Devices::Bluetooth::BluetoothDevice;
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

        // WinRT 要求线程先初始化 COM（MTA）；本线程专用，结束时反初始化。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let body = || -> Result<Vec<String>, String> {
            let selector = BluetoothDevice::GetDeviceSelector().map_err(|e| e.to_string())?;
            let (props, mut out) = choose_aep_props(&selector);
            match enumerate_aep(&selector, &props) {
                Ok(lines) => {
                    out.extend(lines);
                    Ok(out)
                }
                Err(e) => {
                    if props.is_empty() {
                        return Err(e);
                    }
                    out.push(format!("SCANPROBE WinRT-AEP 枚举失败：{e}"));
                    // 降级一：只用核心键（与已选集取交集，已选集本身即核心子集时跳过）
                    let core: Vec<&'static str> = props
                        .iter()
                        .copied()
                        .filter(|k| AEP_PROPS_CORE.contains(k))
                        .collect();
                    if core.len() < props.len() {
                        out.push("SCANPROBE WinRT-AEP 降级：改用核心键组重试".to_string());
                        match enumerate_aep(&selector, &core) {
                            Ok(lines) => {
                                out.extend(lines);
                                return Ok(out);
                            }
                            Err(e2) => {
                                out.push(format!("SCANPROBE WinRT-AEP 核心键组枚举失败：{e2}"))
                            }
                        }
                    }
                    // 降级二：空属性集裸枚举，无论如何要把一节产出来
                    out.push("SCANPROBE WinRT-AEP 降级：改用空属性集重试".to_string());
                    match enumerate_aep(&selector, &[]) {
                        Ok(lines) => {
                            out.extend(lines);
                            Ok(out)
                        }
                        Err(e3) => Err(format!("全部降级均失败，最后错误：{e3}")),
                    }
                }
            }
        };
        let result = body();
        unsafe {
            CoUninitialize();
        }
        result
    }

    /// 用给定属性集跑一轮 DeviceWatcher 枚举并逐项 dump（阻塞，最长约 15 秒）。
    fn enumerate_aep(
        selector: &windows::core::HSTRING,
        props: &[&'static str],
    ) -> Result<Vec<String>, String> {
        use windows::core::{Ref, HSTRING};
        use windows::Devices::Enumeration::{DeviceInformation, DeviceWatcher};
        use windows::Foundation::TypedEventHandler;

        // 申请附加属性：windows 0.61 绑定要 IIterable<HSTRING>，由 Vec 转换
        let props_param: windows_collections::IIterable<HSTRING> = props
            .iter()
            .map(|s| HSTRING::from(*s))
            .collect::<Vec<_>>()
            .into();
        let watcher = DeviceInformation::CreateWatcherAqsFilterAndAdditionalProperties(
            selector,
            &props_param,
        )
        .map_err(|e| e.to_string())?;
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let c2 = collected.clone();
        let dump_props: Vec<&'static str> = props.to_vec();
        watcher
            .Added(&TypedEventHandler::new(
                move |_sender: Ref<DeviceWatcher>, info: Ref<DeviceInformation>| {
                    if let Some(info) = info.as_ref() {
                        // 数据通路：地址 + 信号原始值进缓存，供 list_found 按址并入
                        if let (Some(addr), Some(sig)) = (
                            aep_prop_string(info, "System.Devices.Aep.DeviceAddress"),
                            aep_prop_i32(info, "System.Devices.Aep.SignalStrength"),
                        ) {
                            if let Some(mac) = super::parse_mac(&addr) {
                                if let Ok(mut cache) = AEP_RSSI_CACHE.lock() {
                                    cache.insert(mac, sig);
                                }
                            }
                        }
                        if let Ok(mut g) = c2.lock() {
                            g.extend(dump_aep_device(info, &dump_props));
                        }
                    }
                    Ok(())
                },
            ))
            .map_err(|e| e.to_string())?;
        watcher
            .EnumerationCompleted(&TypedEventHandler::new(
                move |_sender: Ref<DeviceWatcher>, _args: Ref<windows::core::IInspectable>| {
                    let _ = tx.send(());
                    Ok(())
                },
            ))
            .map_err(|e| e.to_string())?;
        watcher.Start().map_err(|e| e.to_string())?;
        let timed_out = rx.recv_timeout(std::time::Duration::from_secs(15)).is_err();
        let _ = watcher.Stop();
        let mut lines = vec![format!(
            "SCANPROBE WinRT-AEP 一节开始（经典设备选择器，申请 {} 项属性）",
            props.len()
        )];
        let count_note;
        if let Ok(mut g) = collected.lock() {
            count_note = g.iter().filter(|l| l.contains("设备开始")).count();
            lines.append(&mut g);
        } else {
            count_note = 0;
        }
        lines.push(format!(
            "SCANPROBE WinRT-AEP 一节结束：共 {count_note} 台{}",
            if timed_out {
                "（15 秒超时未完成，以上为已枚举部分）"
            } else {
                ""
            }
        ));
        Ok(lines)
    }

    /// 把一台 AEP 设备的全部可读信息 dump 成 SCANPROBE 分段行。
    /// props 为本轮实际申请到的属性集，逐项 dump（有值记值、缺失标「（空）」）。
    fn dump_aep_device(
        info: &windows::Devices::Enumeration::DeviceInformation,
        props: &[&str],
    ) -> Vec<String> {
        let id = info.Id().unwrap_or_default();
        let mut lines = vec![format!("SCANPROBE 设备开始 [winrt-aep] {id}")];
        lines.push(format!(
            "SCANPROBE 名称={}",
            info.Name()
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "（空）".to_string())
        ));
        lines.push(format!("SCANPROBE AepId={id}"));
        if let Ok(kind) = info.Kind() {
            lines.push(format!("SCANPROBE 设备种类={kind:?}"));
        }
        if let Ok(pairing) = info.Pairing() {
            if let Ok(v) = pairing.IsPaired() {
                lines.push(format!("SCANPROBE Pairing.IsPaired={v}"));
            }
            if let Ok(v) = pairing.CanPair() {
                lines.push(format!("SCANPROBE Pairing.CanPair={v}"));
            }
        }
        for key in props {
            lines.push(format!("SCANPROBE 属性 {key}={}", fmt_aep_prop(info, key)));
        }
        lines.push(format!("SCANPROBE 设备结束 [winrt-aep] {id}"));
        lines
    }

    /// 读一台设备的一个 AEP 属性并格式化：有值按类型记值，缺失/未填充记「（空）」。
    fn fmt_aep_prop(info: &windows::Devices::Enumeration::DeviceInformation, key: &str) -> String {
        use windows::core::{Interface, HSTRING};
        use windows::Foundation::{IPropertyValue, PropertyType};
        let Ok(map) = info.Properties() else {
            return "（空）".to_string();
        };
        let Ok(val) = map.Lookup(&HSTRING::from(key)) else {
            return "（空）".to_string();
        };
        let Ok(pv) = val.cast::<IPropertyValue>() else {
            return "（非属性值，无法解析）".to_string();
        };
        let Ok(ty) = pv.Type() else {
            return "（空）".to_string();
        };
        match ty {
            PropertyType::Empty => "（空）".to_string(),
            PropertyType::Boolean => pv
                .GetBoolean()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::Int16 => pv
                .GetInt16()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::Int32 => pv
                .GetInt32()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::Int64 => pv
                .GetInt64()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::UInt8 => pv
                .GetUInt8()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::UInt16 => pv
                .GetUInt16()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::UInt32 => pv
                .GetUInt32()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::UInt64 => pv
                .GetUInt64()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::Single => pv
                .GetSingle()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::Double => pv
                .GetDouble()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::String => pv
                .GetString()
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "（空）".into()),
            // Category 是 String[]，展开成一行分号分隔，别落进「未展开」
            // （windows 0.61 的 GetStringArray 是出参式签名）
            PropertyType::StringArray => {
                let mut arr = windows::core::Array::<HSTRING>::new();
                match pv.GetStringArray(&mut arr) {
                    Ok(()) => {
                        let items: Vec<String> = arr.iter().map(|s| s.to_string()).collect();
                        if items.is_empty() {
                            "（空）".to_string()
                        } else {
                            items.join("; ")
                        }
                    }
                    Err(_) => "（空）".to_string(),
                }
            }
            PropertyType::Guid => pv
                .GetGuid()
                .map(|v| format!("{:?}", v))
                .unwrap_or_else(|_| "（空）".into()),
            PropertyType::DateTime => pv
                .GetDateTime()
                .map(|v| format!("UniversalTime={}", v.UniversalTime))
                .unwrap_or_else(|_| "（空）".into()),
            other => format!("（类型 {other:?}，未展开）"),
        }
    }

    /// 读一个 AEP 属性的原始 IPropertyValue（缓存填充用，与 fmt_aep_prop 同读法）。
    fn aep_prop_value(
        info: &windows::Devices::Enumeration::DeviceInformation,
        key: &str,
    ) -> Option<windows::Foundation::IPropertyValue> {
        use windows::core::{Interface, HSTRING};
        let map = info.Properties().ok()?;
        let val = map.Lookup(&HSTRING::from(key)).ok()?;
        val.cast::<windows::Foundation::IPropertyValue>().ok()
    }

    /// 读字符串型 AEP 属性（如 DeviceAddress="c8:94:02:47:20:62"）。
    fn aep_prop_string(
        info: &windows::Devices::Enumeration::DeviceInformation,
        key: &str,
    ) -> Option<String> {
        let pv = aep_prop_value(info, key)?;
        if pv.Type().ok()? != windows::Foundation::PropertyType::String {
            return None;
        }
        pv.GetString().ok().map(|s| s.to_string())
    }

    /// 读整数型 AEP 属性（如 SignalStrength）：按实际数值类型取 i32。
    fn aep_prop_i32(
        info: &windows::Devices::Enumeration::DeviceInformation,
        key: &str,
    ) -> Option<i32> {
        use windows::Foundation::PropertyType;
        let pv = aep_prop_value(info, key)?;
        match pv.Type().ok()? {
            PropertyType::Int16 => pv.GetInt16().ok().map(i32::from),
            PropertyType::Int32 => pv.GetInt32().ok(),
            PropertyType::Int64 => pv
                .GetInt64()
                .ok()
                .map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
            PropertyType::UInt8 => pv.GetUInt8().ok().map(i32::from),
            PropertyType::UInt16 => pv.GetUInt16().ok().map(i32::from),
            PropertyType::UInt32 => pv.GetUInt32().ok().map(|v| v.min(i32::MAX as u32) as i32),
            _ => None,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{unsupported, Conn, FoundDevice, TransportError};
    use tokio::sync::mpsc;

    /// 经典蓝牙中心端（非 Windows 空壳：所有操作返回不支持）。
    pub struct ClassicCentral;

    /// 非 Windows 上经典蓝牙不可用，返回明确错误。
    pub fn classic_central() -> Result<ClassicCentral, TransportError> {
        Err(unsupported())
    }

    impl ClassicCentral {
        pub fn start_scan(&self) -> Result<(), TransportError> {
            Err(unsupported())
        }

        pub fn stop_scan(&self) -> Result<(), TransportError> {
            Err(unsupported())
        }

        pub fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            Err(unsupported())
        }

        pub async fn connect(&self, _device_id: &str) -> Result<Conn, TransportError> {
            Err(unsupported())
        }
    }

    /// 经典蓝牙服务端句柄（非 Windows 空壳）。
    pub struct ClassicServer;

    impl ClassicServer {
        pub fn is_registered(&self) -> bool {
            false
        }

        pub fn stop(&self) {}
    }

    /// 非 Windows 上经典蓝牙不可用，返回明确错误。
    pub fn start_classic_server() -> Result<(ClassicServer, mpsc::Receiver<Conn>), TransportError> {
        Err(unsupported())
    }

    /// AEP 探针仅 Windows 有实现（WinRT），其他平台为空操作。
    pub fn spawn_aep_probe() {}
}

#[cfg(not(windows))]
fn unsupported() -> TransportError {
    TransportError::Ble("经典蓝牙仅支持 Windows".to_string())
}

pub use imp::{
    classic_central, spawn_aep_probe, start_classic_server, ClassicCentral, ClassicServer,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac格式化与解析互逆() {
        // 老版本 formatAddress：高字节在前，冒号分隔，大写十六进制
        assert_eq!(mac_to_string(0xA1B2_C3D4_E5F6), "A1:B2:C3:D4:E5:F6");
        assert_eq!(mac_to_string(0), "00:00:00:00:00:00");
        assert_eq!(parse_mac("A1:B2:C3:D4:E5:F6"), Some(0xA1B2_C3D4_E5F6));
        assert_eq!(parse_mac("a1:b2:c3:d4:e5:f6"), Some(0xA1B2_C3D4_E5F6));
        assert_eq!(parse_mac("A1:B2:C3"), None);
        assert_eq!(parse_mac("ZZ:B2:C3:D4:E5:F6"), None);
    }

    #[test]
    fn 设备id带前缀且可还原() {
        let id = device_id(0xA1B2_C3D4_E5F6);
        assert_eq!(id, "classic:A1:B2:C3:D4:E5:F6");
        assert_eq!(mac_from_device_id(&id), Some(0xA1B2_C3D4_E5F6));
        // 裸 MAC 也接受
        assert_eq!(
            mac_from_device_id("A1:B2:C3:D4:E5:F6"),
            Some(0xA1B2_C3D4_E5F6)
        );
        assert_eq!(mac_from_device_id("ble-device-id"), None);
    }

    #[test]
    fn 非windows平台报告不支持() {
        if !cfg!(windows) {
            assert!(!classic_supported());
            assert!(classic_central().is_err());
            assert!(start_classic_server().is_err());
        }
    }

    fn cdev(mac: u64, rssi: Option<i16>) -> FoundDevice {
        FoundDevice {
            id: device_id(mac),
            name: "LAPTOP-X".to_string(),
            rssi,
            paired: true,
        }
    }

    /// AEP 合并：绝对 dBm（-42）并入；相对刻度（5）被护栏挡住不画格；
    /// 查不到的设备保持无信号；已有信号不被覆盖。地址对齐靠 MAC 数值，
    /// AEP 的小写地址与经典 id 的大写形式经 parse_mac 归一（互逆测试已覆盖）。
    #[test]
    fn aep信号合并与刻度护栏() {
        let mac_a = 0xC894_0247_2062; // 真机样本 LAPTOP-QN3CQGC7，未连接 -42
        let mac_b = 0x34C9_3DAC_1E57; // 真机样本 LAPTOP-VE2P2MLC，已连接 5
        let mac_c = 0x0011_2233_4455; // AEP 里没有的陌生设备
        let cache: std::collections::HashMap<u64, i32> =
            [(mac_a, -42), (mac_b, 5)].into_iter().collect();
        let mut devs = vec![
            cdev(mac_a, None),
            cdev(mac_b, None),
            cdev(mac_c, None),
            cdev(mac_a, Some(-60)),
        ];
        merge_aep_rssi(&mut devs, &|mac| cache.get(&mac).copied());
        assert_eq!(devs[0].rssi, Some(-42));
        assert_eq!(devs[1].rssi, None); // 5 > -25：护栏挡住
        assert_eq!(devs[2].rssi, None); // 无数据：保持无信号
        assert_eq!(devs[3].rssi, Some(-60)); // 已有值不覆盖
                                             // 护栏边界：-25 并入、-24 挡住
        let cache2: std::collections::HashMap<u64, i32> =
            [(mac_a, -25), (mac_b, -24)].into_iter().collect();
        let mut devs2 = vec![cdev(mac_a, None), cdev(mac_b, None)];
        merge_aep_rssi(&mut devs2, &|mac| cache2.get(&mac).copied());
        assert_eq!(devs2[0].rssi, Some(-25));
        assert_eq!(devs2[1].rssi, None);
    }

    /// 在场门三类设备（2026-11-20 v2.5.1）：本轮刚听到→进；九个月前最后一次
    /// 见到的记住设备（用户实证 LAPTOP-QN3CQGC7 lastSeen=2026-02-24）→出；
    /// 已连接→进（哪怕 lastSeen 旧）；无 lastSeen 且未连接→出；容差边界内→进。
    #[test]
    fn 在场门三类设备() {
        // 2026-11-19 09:30:00 UTC 一轮扫描开始（unix 秒经 date 核对）
        let round_start = 1_795_080_600_i64;
        // 本轮内刚听到（晚于轮始 3 秒）
        assert!(heard_this_round(Some(round_start + 3), false, round_start));
        // 容差边界：轮始前 2 秒内仍算本轮（容差常量 2）
        assert!(heard_this_round(Some(round_start - 2), false, round_start));
        // 轮始前 3 秒：出
        assert!(!heard_this_round(Some(round_start - 3), false, round_start));
        // 九个月前（2026-02-24 12:00 UTC = 1771934400，用户实证 QN3CQGC7）：出
        assert!(!heard_this_round(Some(1_771_934_400), false, round_start));
        // lastSeen 旧但当前已连接：进
        assert!(heard_this_round(Some(1_771_934_400), true, round_start));
        // 没有 lastSeen 且未连接（纯记住条目）：出
        assert!(!heard_this_round(None, false, round_start));
        // 没有 lastSeen 但已连接：进
        assert!(heard_this_round(None, true, round_start));
        // civil→unix 换算自检（数值经 date -u 核对）
        assert_eq!(civil_to_unix_secs(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(
            civil_to_unix_secs(2026, 2, 24, 12, 0, 0),
            Some(1_771_934_400)
        );
        assert_eq!(
            civil_to_unix_secs(2026, 11, 19, 9, 30, 0),
            Some(1_795_080_600)
        );
    }
}
