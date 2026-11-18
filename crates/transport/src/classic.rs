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

#[cfg(windows)]
mod imp {
    // Win32 FFI 结构体（SOCKADDR_BTH / WSAQUERYSETW 等）惯例是先全零再逐字段赋值，
    // 比结构体字面量更不易漏字段，故本模块豁免这条 clippy 规则。
    #![allow(clippy::field_reassign_with_default)]
    use super::{device_id, mac_from_device_id, mac_to_string, Conn, FoundDevice, TransportError};
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
                while scanning.load(Ordering::SeqCst) && generation.load(Ordering::SeqCst) == gen {
                    // 单轮失败（适配器忙等）不终止扫描，下一轮重试——与 BLE 轮询容错一致
                    if let Ok(devs) = run_inquiry() {
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

        /// 最新一轮 Inquiry 的设备快照（不过滤：全部经典设备，与老版本一致；
        /// 是不是 BlueLink 由连接后的服务层握手判定）。
        pub fn list_found(&self) -> Result<Vec<FoundDevice>, TransportError> {
            Ok(self.found.lock().map(|f| f.clone()).unwrap_or_default())
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
    fn run_inquiry() -> Result<Vec<FoundDevice>, TransportError> {
        unsafe {
            let (find_radio, radio) = first_radio()?;
            let result = inquiry_on_radio(radio);
            close_radio(find_radio, radio);
            result
        }
    }

    /// # Safety
    /// radio 必须是有效的蓝牙适配器句柄。
    unsafe fn inquiry_on_radio(radio: HANDLE) -> Result<Vec<FoundDevice>, TransportError> {
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
            return Ok(Vec::new());
        }
        let mut out: Vec<FoundDevice> = Vec::new();
        loop {
            let mac = info.Address.Anonymous.ullLong;
            let id = device_id(mac);
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
                });
            }
            info = BLUETOOTH_DEVICE_INFO::default();
            info.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
            if BluetoothFindNextDevice(find, &mut info) == 0 {
                break;
            }
        }
        BluetoothFindDeviceClose(find);
        Ok(out)
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
}

#[cfg(not(windows))]
fn unsupported() -> TransportError {
    TransportError::Ble("经典蓝牙仅支持 Windows".to_string())
}

pub use imp::{classic_central, start_classic_server, ClassicCentral, ClassicServer};

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
}
