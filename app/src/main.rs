// BlueLink 桌面应用（Tauri 2）：命令层把前端调用转给服务层节点，
// 事件层把节点事件转发给前端。命令清单与详细设计第三章对应。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod local_api;
mod win_clipboard;
#[cfg(windows)]
mod win_notify;

use bluelink_ai::{AiClient, AiConfig, ChatMessage};
use bluelink_domain::{
    CategorizedMessage, Category, Conversation, MarkKind, Message, MessageCategory, Tag,
};
use bluelink_services::{Event, Node};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, Mutex};

/// 应用状态：持有节点、AI 任务取消通道与蓝牙运行时（中心端 + 服务端）。
/// 蓝牙不可用（无适配器/未开启）时对应字段为 None，界面据 bt_state 如实展示。
struct AppState {
    node: Arc<Node>,
    data_dir: std::path::PathBuf,
    ai_cancel: Mutex<Option<oneshot::Sender<()>>>,
    central: Option<Arc<bluelink_transport::ble::BleCentral>>,
    server: Option<Arc<bluelink_transport::ble::BleServer>>,
    /// 蓝牙广播（服务端）启动失败时的原始错误，界面据此如实提示原因。
    server_error: Option<String>,
    /// 蓝牙广播未启动但不是失败（如启动时选了 SimpleBLE 而主动跳过）时的说明，
    /// 界面据此如实展示，别让用户对着「未启动」猜原因。
    server_note: Option<String>,
    /// 连接尝试令牌（取消机制）：每次发起连接领取一个新代号，点「取消」即
    /// 把代号作废；连接命令在关键节点核对代号，作废的尝试即使迟到成功
    /// 也必须自行断开、不进会话（经典 connect 阻塞在系统调用里掐不断，
    /// 靠这套令牌保证状态机立刻响应、迟到结果被丢弃）。
    connect_gen: std::sync::atomic::AtomicU64,
    /// 经典蓝牙运行时（仅 Windows）：Inquiry 中心端 + RFCOMM 服务端，与 BLE 并行。
    classic_central: Option<Arc<bluelink_transport::classic::ClassicCentral>>,
    classic_server: Option<Arc<bluelink_transport::classic::ClassicServer>>,
    /// 经典蓝牙服务端启动失败时的原始错误，界面据此如实提示原因。
    classic_server_error: Option<String>,
    /// SimpleBLE 运行时（第三个蓝牙实现，开发者模式可选）：中心端在编译了
    /// simpleble feature 时创建；服务端仅在启动时设置已选 simpleble 才拉起
    /// （避免与 BLE 服务端同时抢同一适配器的外设角色）。
    sble_central: Option<Arc<bluelink_transport::simpleble::SbleCentral>>,
    sble_server: Option<Arc<bluelink_transport::simpleble::SbleServer>>,
    /// SimpleBLE 服务端启动失败时的原始错误，界面据此如实提示原因。
    sble_server_error: Option<String>,
    /// SimpleBLE 中心端初始化失败时的原始错误：与「未编译」区分——编译了但
    /// 初始化失败时扫描/连接要如实回落并在日志与界面写明是哪一种（用户真机
    /// 实证：另一台机器选 SimpleBLE「跑不起来」时旧版无声无息，无从诊断）。
    sble_central_error: Option<String>,
    scan_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    listening: Mutex<Option<tokio::task::JoinHandle<()>>>,
    local_api: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

/// 本机身份信息（连接页与设置页用）。
#[derive(Serialize)]
struct Identity {
    device_id: String,
    name: String,
    code: String,
    code_display: String,
    download_dir: String,
    connected: bool,
}

async fn identity_of(node: &Arc<Node>) -> Identity {
    Identity {
        device_id: node.device_id.clone(),
        name: node.device_name.clone(),
        code: node.my_code(),
        code_display: bluelink_domain::format_code(&node.my_code()),
        download_dir: node.download_dir().to_string_lossy().to_string(),
        connected: node.is_connected().await,
    }
}

#[tauri::command]
async fn get_identity(state: State<'_, AppState>) -> Result<Identity, String> {
    Ok(identity_of(&state.node).await)
}

/// 蓝牙状态（连接页展示）：适配器是否可用、广播（可被发现）是否已开启；
/// classic_* 三个字段是经典蓝牙通道的同构状态（仅 Windows 有意义，
/// 前端据 classic_available 决定是否显示「经典蓝牙 / BLE」连接方式选择）。
#[derive(Serialize)]
struct BtState {
    available: bool,
    advertising: bool,
    message: String,
    classic_available: bool,
    classic_serving: bool,
    classic_message: String,
    /// SimpleBLE 状态（开发者模式切换时展示）：compiled 为 false 表示此版本
    /// 构建时未开启 simpleble feature，选了也会回落经典蓝牙，message 里写明。
    simpleble_compiled: bool,
    simpleble_available: bool,
    simpleble_serving: bool,
    simpleble_message: String,
}

#[tauri::command]
async fn bt_state(state: State<'_, AppState>) -> Result<BtState, String> {
    let available = state.central.is_some();
    // start_ble_server 只有在广播真正启动后才返回 Ok，server 存在即正在广播
    let advertising = state.server.is_some();
    let message = if !available {
        "未检测到蓝牙适配器或蓝牙未开启".to_string()
    } else if advertising {
        "蓝牙已开启，本机可被附近设备发现".to_string()
    } else if let Some(err) = &state.server_error {
        format!("蓝牙已开启，但本机广播启动失败：{err}")
    } else if let Some(note) = &state.server_note {
        note.clone()
    } else {
        "蓝牙已开启，但本机广播未启动".to_string()
    };
    let classic_available = state.classic_central.is_some();
    // start_classic_server 同步完成 bind/listen 才返回 Ok，server 存在即在监听
    let classic_serving = state.classic_server.is_some();
    let classic_message = if !bluelink_transport::classic::classic_supported() {
        "经典蓝牙仅支持 Windows".to_string()
    } else if !classic_available {
        "未检测到蓝牙适配器或蓝牙未开启".to_string()
    } else if classic_serving {
        if state
            .classic_server
            .as_ref()
            .map(|s| s.is_registered())
            .unwrap_or(false)
        {
            "经典蓝牙已开启，本机可被附近设备发现".to_string()
        } else {
            "经典蓝牙服务已启动，但 SDP 注册失败，对方可能找不到本机".to_string()
        }
    } else if let Some(err) = &state.classic_server_error {
        if err.contains("10048") {
            // 端口 7 已被占用（多半是另一个 BlueLink 或老版本在运行）：给人话说明，
            // 原始错误已在启动日志里完整记录（setup 里的「经典蓝牙服务端启动失败」一行）。
            "经典蓝牙服务未能启动：端口被其他程序占用（多半是另一个 BlueLink 或老版本正在运行），关掉它再重启本应用；你主动连接别人不受影响"
                .to_string()
        } else {
            format!("经典蓝牙已开启，但服务启动失败：{err}")
        }
    } else {
        "经典蓝牙已开启，但服务未启动".to_string()
    };
    let simpleble_compiled = bluelink_transport::simpleble::simpleble_compiled();
    let simpleble_available = state.sble_central.is_some();
    // start_sble_server 在广播真正启动后才返回 Ok，server 存在即正在广播
    let simpleble_serving = state.sble_server.is_some();
    let simpleble_message = if !simpleble_compiled {
        "SimpleBLE 未编译进此版本（构建时未开启 simpleble feature），已回落经典蓝牙".to_string()
    } else if let Some(err) = &state.sble_central_error {
        // 已编译但初始化失败：与「未检测到蓝牙」严格区分，带原始错误 + 回落去向
        format!("SimpleBLE 启动失败：{err}，已回落经典蓝牙")
    } else if !simpleble_available {
        "未检测到蓝牙适配器或蓝牙未开启".to_string()
    } else if simpleble_serving {
        "SimpleBLE 已开启，本机可被附近设备发现".to_string()
    } else if let Some(err) = &state.sble_server_error {
        format!("SimpleBLE 已开启，但广播启动失败：{err}")
    } else {
        "SimpleBLE 可用，但本机广播未启动（切换到 SimpleBLE 后重启应用才会被对方发现；主动扫描连接不受影响）"
            .to_string()
    };
    Ok(BtState {
        available,
        advertising,
        message,
        classic_available,
        classic_serving,
        classic_message,
        simpleble_compiled,
        simpleble_available,
        simpleble_serving,
        simpleble_message,
    })
}

/// 扫描中心端的统一视图：经典蓝牙（Inquiry）、BLE 与 SimpleBLE 三套中心端
/// 方法签名对齐，扫描轮询逻辑共用一份。transport 参数由连接页的连接方式
/// 选择传入（"classic" / "ble" / "simpleble"，经 resolve_transport 归一化）。
enum ScanCentral {
    Ble(Arc<bluelink_transport::ble::BleCentral>),
    Classic(Arc<bluelink_transport::classic::ClassicCentral>),
    Simpleble(Arc<bluelink_transport::simpleble::SbleCentral>),
}

impl ScanCentral {
    async fn start_scan(&self) -> Result<(), String> {
        match self {
            ScanCentral::Ble(c) => c.start_scan().await.map_err(|e| e.to_string()),
            ScanCentral::Classic(c) => c.start_scan().map_err(|e| e.to_string()),
            ScanCentral::Simpleble(c) => c.start_scan().await.map_err(|e| e.to_string()),
        }
    }

    async fn list_found(&self) -> Result<Vec<bluelink_transport::ble::FoundDevice>, String> {
        match self {
            ScanCentral::Ble(c) => c.list_found().await.map_err(|e| e.to_string()),
            ScanCentral::Classic(c) => c.list_found().map_err(|e| e.to_string()),
            ScanCentral::Simpleble(c) => c.list_found().await.map_err(|e| e.to_string()),
        }
    }

    fn log_tag(&self) -> &'static str {
        match self {
            ScanCentral::Ble(_) => "ble",
            ScanCentral::Classic(_) => "classic",
            ScanCentral::Simpleble(_) => "simpleble",
        }
    }
}

/// 把前端请求的传输归一化为实际生效的传输。
/// 关键回落：请求 simpleble 但 SimpleBLE 用不了时，绝不假装在用 SimpleBLE——
/// 回落经典蓝牙（无经典通道的平台回落 BLE），并在运行日志里如实写明原因
/// （bt_state 的 simpleble_message 同步展示）。用不了分两种，日志必须分清：
/// ①未编译（feature 未开启）；②已编译但中心端初始化失败（原始错误见
/// sble_central_error，启动时已记一行「SimpleBLE 中心端不可用」）。
fn resolve_transport(state: &AppState, requested: &str) -> &'static str {
    match requested {
        "classic" => "classic",
        "simpleble" => {
            let compiled = bluelink_transport::simpleble::simpleble_compiled();
            if compiled && state.sble_central.is_some() {
                "simpleble"
            } else {
                let reason = if compiled {
                    format!(
                        "SimpleBLE 初始化失败（{}）",
                        state.sble_central_error.as_deref().unwrap_or("未知原因")
                    )
                } else {
                    "此版本未编译 SimpleBLE（构建时未开启 simpleble feature）".to_string()
                };
                if state.classic_central.is_some() {
                    bluelink_services::log_line(
                        "simpleble",
                        &format!("设置选择了 SimpleBLE，但{reason}，已回落经典蓝牙"),
                    );
                    "classic"
                } else {
                    bluelink_services::log_line(
                        "simpleble",
                        &format!("设置选择了 SimpleBLE，但{reason}，已回落 BLE"),
                    );
                    "ble"
                }
            }
        }
        _ => "ble",
    }
}

/// 开始扫描附近的设备：结果经 bt_devices 事件每秒推送快照。
/// transport 先经 resolve_transport 归一化："classic" 走经典 Inquiry
/// （不过滤，含已配对/记住的设备），"simpleble" 走 SimpleBLE 扫描
/// （未编译时回落并记日志），其余值走 BLE 扫描（按 BlueLink 服务过滤）。
#[tauri::command]
async fn bt_start_scan(
    state: State<'_, AppState>,
    app: AppHandle,
    transport: String,
) -> Result<(), String> {
    let central = match resolve_transport(&state, &transport) {
        "classic" => ScanCentral::Classic(
            state
                .classic_central
                .clone()
                .ok_or_else(|| "经典蓝牙不可用".to_string())?,
        ),
        "simpleble" => ScanCentral::Simpleble(
            state
                .sble_central
                .clone()
                .ok_or_else(|| "SimpleBLE 不可用".to_string())?,
        ),
        _ => ScanCentral::Ble(
            state
                .central
                .clone()
                .ok_or_else(|| "蓝牙不可用".to_string())?,
        ),
    };
    // 上一轮扫描可能有没写完的 SCANPROBE 诊断行，先补写
    for line in bluelink_transport::take_probe_lines() {
        bluelink_services::log::log_verbose("classic", &line);
    }
    let is_classic = matches!(central, ScanCentral::Classic(_));
    let tag = central.log_tag();
    if let Err(e) = central.start_scan().await {
        // 启动失败必须留痕（旧版静默返回，前端只剩一句无头绪的错误）
        bluelink_services::log_line(tag, &format!("扫描启动失败：{e}"));
        return Err(e);
    }
    if is_classic {
        // 经典扫描顺带跑一次 AEP 探针（仅日志：DeviceWatcher 枚举经典设备
        // 并 dump AEP 属性全集，定案 SignalStrength 对经典是否填充；节流与
        // 实现在 transport 内，结果由下方轮询逐行写进运行日志）
        bluelink_transport::classic::spawn_aep_probe();
    }
    if let Some(h) = state.scan_task.lock().await.take() {
        h.abort();
    }
    let handle = tokio::spawn(async move {
        // 偶发读取错误不终止扫描（否则界面还显示「正在扫描」实际已停）；连续失败多次才停
        let mut consecutive_errors = 0u32;
        loop {
            // SCANPROBE 诊断行随扫描轮询写出（日志里搜「SCANPROBE」可定位）
            for line in bluelink_transport::take_probe_lines() {
                bluelink_services::log::log_verbose(tag, &line);
            }
            match central.list_found().await {
                Ok(devices) => {
                    consecutive_errors = 0;
                    let _ = app.emit("bt_devices", devices);
                }
                Err(e) => {
                    consecutive_errors += 1;
                    bluelink_services::log_line(
                        tag,
                        &format!("扫描读取设备列表失败（第 {consecutive_errors} 次）：{e}"),
                    );
                    if consecutive_errors >= 5 {
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    *state.scan_task.lock().await = Some(handle);
    Ok(())
}

/// 停止扫描（三套中心端都停，互不影响）。
#[tauri::command]
async fn bt_stop_scan(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(h) = state.scan_task.lock().await.take() {
        h.abort();
    }
    if let Some(c) = &state.central {
        let _ = c.stop_scan().await;
    }
    if let Some(c) = &state.classic_central {
        let _ = c.stop_scan();
    }
    if let Some(c) = &state.sble_central {
        let _ = c.stop_scan().await;
    }
    Ok(())
}

/// 开发者模式开关（持久化在设置里，默认关）。
/// 局域网通道只供开发测试：正式版默认关闭，命令层也会拒绝未开启时的局域网调用。
fn dev_mode_on(state: &AppState) -> bool {
    state
        .node
        .storage
        .get_setting("dev_mode")
        .map(|v| v.as_deref() == Some("1"))
        .unwrap_or(false)
}

#[tauri::command]
fn get_dev_mode(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(dev_mode_on(&state))
}

#[tauri::command]
fn set_dev_mode(state: State<'_, AppState>, on: bool) -> Result<(), String> {
    state
        .node
        .storage
        .set_setting("dev_mode", if on { "1" } else { "0" })
        .map_err(|e| e.to_string())?;
    // 日志分级即时生效：开发者模式 = 全量日志（verbose），关 = 只记必要日志
    bluelink_services::log::set_verbose(on);
    Ok(())
}

/// 读取开发者模式里选定的蓝牙传输（设置键 bt_transport）：classic / ble / simpleble。
/// 普通模式不读它（连接页固定经典蓝牙），仅开发者模式开启时前端据此切换通道。
#[tauri::command]
fn get_bt_transport(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state
        .node
        .storage
        .get_setting("bt_transport")
        .map_err(|e| e.to_string())?
        .filter(|v| matches!(v.as_str(), "classic" | "ble" | "simpleble"))
        .unwrap_or_else(|| "classic".to_string()))
}

/// 保存开发者模式里选定的蓝牙传输；非法值一律落回 classic（默认）。
#[tauri::command]
fn set_bt_transport(state: State<'_, AppState>, transport: String) -> Result<(), String> {
    let v = match transport.as_str() {
        "ble" => "ble",
        "simpleble" => "simpleble",
        _ => "classic",
    };
    state
        .node
        .storage
        .set_setting("bt_transport", v)
        .map_err(|e| e.to_string())
}

fn require_dev_mode(state: &AppState) -> Result<(), String> {
    if dev_mode_on(state) {
        Ok(())
    } else {
        Err("局域网通道仅在开发者模式下可用（设置中开启）".into())
    }
}

/// 【开发者模式】开始监听：局域网 TCP 等待连接（仅开发测试，正式版默认不可见）。
#[tauri::command]
async fn start_listening(state: State<'_, AppState>, port: Option<u16>) -> Result<String, String> {
    require_dev_mode(&state)?;
    let port = port.unwrap_or(7800);
    let addr = format!("0.0.0.0:{port}");
    let listener = bluelink_transport::listen_tcp(&addr)
        .await
        .map_err(|e| e.to_string())?;
    let node = state.node.clone();
    let handle = tokio::spawn(async move {
        if let Ok(conn) = bluelink_transport::accept_tcp(&listener).await {
            let _ = node.attach(conn, true, "").await;
        }
    });
    *state.listening.lock().await = Some(handle);
    Ok(addr)
}

/// 【开发者模式】发起局域网连接：addr 形如 192.168.1.10:7800（仅开发测试）。
#[tauri::command]
async fn connect_to(
    state: State<'_, AppState>,
    addr: String,
    code: String,
) -> Result<String, String> {
    require_dev_mode(&state)?;
    let gen = state.begin_connect();
    let conn = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        bluelink_transport::connect_tcp(addr.trim()),
    )
    .await
    .map_err(|_| {
        // 失败定案必须落日志（essential，两级日志都要有）——旧版只回前端，
        // 日志里查不到这次失败（v2.5.1 实测缺口）。
        bluelink_services::log_line("app", "局域网连接失败：连接超时（20 秒），对方未响应");
        "连接超时（20 秒）：对方未响应".to_string()
    })?
    .map_err(|e| {
        let m = format!("连接失败：{e}");
        bluelink_services::log_line("app", &format!("局域网{m}"));
        m
    })?;
    // 取消核对（一）：TCP 已连上但尝试已被取消——直接丢弃，不进握手
    if !state.connect_current(gen) {
        drop(conn);
        return Err("连接已取消".into());
    }
    state
        .node
        .set_pending_route(format!("tcp:{}", addr.trim()))
        .await;
    let code_clean: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let peer = state
        .node
        .attach(conn, false, &code_clean)
        .await
        .map_err(|e| {
            let m = e.to_string();
            bluelink_services::log_line("app", &format!("局域网连接失败（握手阶段）：{m}"));
            m
        })?;
    // 取消核对（二）：握手期间被取消——立即断开，不进会话
    if !state.connect_current(gen) {
        state.node.disconnect().await;
        return Err("连接已取消".into());
    }
    Ok(peer.name)
}

/// 读取上次成功连接的设备（JSON 字符串，存设置表 last_peer 键；前端负责序列化）。
/// 连接页据此渲染「上次连接」一键重连入口（用户 2026-11-18 要求：记住上次设备、点一下直接连）。
#[tauri::command]
async fn get_last_peer(state: State<'_, AppState>) -> Result<Option<String>, String> {
    state
        .node
        .storage
        .get_setting("last_peer")
        .map_err(|e| e.to_string())
}

/// 保存上次连接设备（json 为空串 = 清除记录）。
#[tauri::command]
async fn set_last_peer(state: State<'_, AppState>, json: String) -> Result<(), String> {
    state
        .node
        .storage
        .set_setting("last_peer", &json)
        .map_err(|e| e.to_string())
}

/// 连接扫描到的设备（访客模式）：code 为对方屏幕上显示的连接码，握手时校验。
/// transport 与扫描时一致（经 resolve_transport 归一化）："classic" 经 SDP
/// 连经典蓝牙，"simpleble" 走 SimpleBLE（未编译时回落），其余走 BLE。
#[tauri::command]
async fn bt_connect(
    state: State<'_, AppState>,
    device_id: String,
    code: String,
    transport: String,
    name: String,
    rssi: Option<i32>,
) -> Result<String, String> {
    let gen = state.begin_connect();
    let resolved = resolve_transport(&state, &transport);
    // 目标身份记全（名称/地址/信号）：列表里同名重复时，凭这行就能断定
    // 用户点到的是哪条身份（真身还是已轮换走的旧地址）。
    bluelink_services::log_line(
        resolved,
        &format!(
            "开始连接：目标 {name}（{device_id}，信号 {}，请求传输 {transport}，实际 {resolved}）",
            rssi.map(|v| format!("{v} dBm"))
                .unwrap_or_else(|| "无信号值".to_string())
        ),
    );
    let code_clean: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    // 双边同连仲裁登记（v2.6.1）：握手期间若对方同时连进来，服务层按
    // 设备 ID 字典序裁决让行/拒绝（见 services 的 handshake）。整个连接
    // 流程（含可能的重试）结束才注销。
    let out_gen = state.node.begin_outbound_attempt(&name).await;
    let result = bt_connect_once(&state, gen, resolved, &device_id, &code_clean).await;
    // SimpleBLE「bad exception」= 适配器状态损坏（v2.6.5，#18 日志定案）：
    // 用同一坏句柄重试只会连环失败——先重初始化适配器、重新扫描捕获
    // 目标，再重试一次。
    let result = match result {
        Err(e)
            if resolved == "simpleble"
                && state.connect_current(gen)
                && e.to_lowercase().contains("bad exception") =>
        {
            bluelink_services::log_line(
                resolved,
                &format!(
                    "首次连接失败（适配器状态损坏：{e}），重初始化适配器并重新捕获后自动重试一次"
                ),
            );
            let mut retried = None;
            if let Some(central) = state.sble_central.clone() {
                central.reinit().await;
                if let Some(fresh_id) = rescan_fresh_id(&state, resolved, &device_id, &name).await {
                    retried =
                        Some(bt_connect_once(&state, gen, resolved, &fresh_id, &code_clean).await);
                }
            }
            match retried {
                Some(r) => r,
                None => Err(e),
            }
        }
        other => other,
    };
    let result = match result {
        Err(e)
            if matches!(resolved, "ble" | "simpleble")
                && state.connect_current(gen)
                && is_stale_endpoint_failure(&e) =>
        {
            // 缓存过期家族（v2.6.1，2026-11-21 双机日志定案）：对端重启后
            // 本机连到缓存里的旧端点——无线电/GATT 能通但对端应用层零
            // 入站，握手必然超时。清缓存 + 强制重扫拿到新地址后自动重试
            // 一次，不让用户手动重扫重连。
            bluelink_services::log_line(
                resolved,
                &format!("首次连接失败（疑似缓存旧端点：{e}），清缓存并重新扫描后自动重试一次"),
            );
            match rescan_fresh_id(&state, resolved, &device_id, &name).await {
                Some(fresh_id) => {
                    bluelink_services::log_line(
                        resolved,
                        &format!("重扫已定位目标新地址 {fresh_id}，开始重试"),
                    );
                    bt_connect_once(&state, gen, resolved, &fresh_id, &code_clean).await
                }
                None => {
                    bluelink_services::log_line(resolved, "重扫 8 秒内未再发现目标设备，放弃重试");
                    Err(e)
                }
            }
        }
        other => other,
    };
    state.node.end_outbound_attempt(out_gen).await;
    // 双边同连让行不是失败：本机出站虽收尾，但对方连入的链路已把会话
    // 建立起来——按成功返回（目标名即用户点的那台，不会记错上次连接）。
    if let Err(e) = &result {
        if e.contains("已让行入站连接") && state.node.is_connected().await {
            return Ok(name);
        }
    }
    result
}

/// 是否为「缓存旧端点」家族的失败：握手超时（连到不应答的旧端点）、
/// 握手期连接被关（旧端点残链）、步骤1 等 10 秒仍不在扫描缓存。
/// 都是「地址/缓存过期」的一种表现，清缓存重扫后值得自动重试一次；
/// 连接码错误、用户取消等不在此列，重试只会再错一次。
fn is_stale_endpoint_failure(err: &str) -> bool {
    err.contains("握手超时") || err.contains("不在扫描范围内") || err.contains("连接已关闭")
}

/// 清缓存 + 强制重新扫描，按设备名找回目标的新鲜条目（最长等 8 秒）。
/// BLE 与 SimpleBLE 各走自己的中心端；经典蓝牙不经此路（无此缓存问题）。
async fn rescan_fresh_id(
    state: &AppState,
    resolved: &str,
    old_id: &str,
    name: &str,
) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    match resolved {
        "simpleble" => {
            let central = state.sble_central.clone()?;
            central.forget_device(old_id).await;
            let _ = central.start_scan().await;
            loop {
                if let Ok(devices) = central.list_found().await {
                    if let Some(d) = devices.iter().find(|d| d.name == name) {
                        return Some(d.id.clone());
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        _ => {
            let central = state.central.clone()?;
            central.forget_device(old_id).await;
            let _ = central.start_scan().await;
            loop {
                if let Ok(devices) = central.list_found().await {
                    if let Some(d) = devices.iter().find(|d| d.name == name) {
                        return Some(d.id.clone());
                    }
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

/// 一次完整的连接尝试：传输连接 → 取消核对 → 握手接管 → 取消核对。
/// bt_connect 的重试逻辑复用它（参数与原 bt_connect 主体一致）。
async fn bt_connect_once(
    state: &AppState,
    gen: u64,
    resolved: &str,
    device_id: &str,
    code_clean: &str,
) -> Result<String, String> {
    // 自动重连路线登记（v2.6.2）：attach 成功时被服务层消费为回拨
    // 目标；连接失败则留着也无妨（下次 attach 前会被覆盖）。
    state
        .node
        .set_pending_route(format!("bt:{resolved}:{device_id}"))
        .await;
    let conn = match resolved {
        "classic" => {
            let central = state
                .classic_central
                .clone()
                .ok_or_else(|| "经典蓝牙不可用".to_string())?;
            central.connect(device_id).await.map_err(|e| {
                let m = format!("经典蓝牙连接失败：{e}");
                bluelink_services::log_line("app", &m);
                m
            })?
        }
        "simpleble" => {
            let central = state
                .sble_central
                .clone()
                .ok_or_else(|| "SimpleBLE 不可用".to_string())?;
            central.connect(device_id).await.map_err(|e| {
                let m = format!("SimpleBLE 连接失败：{e}");
                bluelink_services::log_line("app", &m);
                m
            })?
        }
        _ => {
            let central = state
                .central
                .clone()
                .ok_or_else(|| "蓝牙不可用".to_string())?;
            central.connect(device_id).await.map_err(|e| {
                let m = format!("蓝牙连接失败：{e}");
                bluelink_services::log_line("app", &m);
                m
            })?
        }
    };
    // 取消核对（一）：传输层已连上但尝试已被取消（经典 connect 可能阻塞很久
    // 才迟到成功）——直接丢弃连接，不进握手
    if !state.connect_current(gen) {
        drop(conn);
        return Err("连接已取消".into());
    }
    let peer = state
        .node
        .attach(conn, false, code_clean)
        .await
        .map_err(|e| {
            let m = e.to_string();
            bluelink_services::log_line("app", &format!("蓝牙连接失败（握手阶段）：{m}"));
            m
        })?;
    // 取消核对（二）：握手期间被取消——立即断开，不进会话
    if !state.connect_current(gen) {
        state.node.disconnect().await;
        return Err("连接已取消".into());
    }
    Ok(peer.name)
}

/// 自动重连拨号器（v2.6.2）：服务层在意外断线时回调。按 attach 时
/// 登记的路线重新拨号——"tcp:<addr>" 走局域网（开发者模式测试路），
/// "bt:<kind>:<扫描id>" 走对应蓝牙中心端；BLE/SimpleBLE 首次失败时
/// 按「缓存旧端点」家族处理：清缓存、按设备名重扫找回、重试一次
/// （与 bt_connect 的手动路径同策略）。握手沿用连接码校验（attach）。
fn make_reconnect_dialer(
    central: Option<std::sync::Arc<bluelink_transport::ble::BleCentral>>,
    classic: Option<std::sync::Arc<bluelink_transport::classic::ClassicCentral>>,
    sble: Option<std::sync::Arc<bluelink_transport::simpleble::SbleCentral>>,
) -> bluelink_services::ReconnectDialer {
    std::sync::Arc::new(move |node, target, gen| {
        let (central, classic, sble) = (central.clone(), classic.clone(), sble.clone());
        Box::pin(async move {
            let conn = dial_reconnect_route(&central, &classic, &sble, &target).await?;
            if !node.reconnect_should_continue(gen) {
                drop(conn);
                return Err("重连已被取消".to_string());
            }
            node.attach(conn, false, &target.code)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    })
}

async fn dial_reconnect_route(
    central: &Option<std::sync::Arc<bluelink_transport::ble::BleCentral>>,
    classic: &Option<std::sync::Arc<bluelink_transport::classic::ClassicCentral>>,
    sble: &Option<std::sync::Arc<bluelink_transport::simpleble::SbleCentral>>,
    target: &bluelink_services::ReconnectTarget,
) -> Result<bluelink_transport::Conn, String> {
    if let Some(addr) = target.route.strip_prefix("tcp:") {
        return bluelink_transport::connect_tcp(addr)
            .await
            .map_err(|e| e.to_string());
    }
    let rest = target
        .route
        .strip_prefix("bt:")
        .ok_or_else(|| "未知重连路线".to_string())?;
    let (kind, id) = rest
        .split_once(':')
        .ok_or_else(|| "重连路线格式错误".to_string())?;
    match kind {
        "classic" => {
            let c = classic.clone().ok_or("经典蓝牙不可用")?;
            c.connect(id).await.map_err(|e| e.to_string())
        }
        "simpleble" => {
            let c = sble.clone().ok_or("SimpleBLE 不可用")?;
            // #19 捕获先行：先弃缓存句柄、重新扫描按名捕获新鲜条目再连，
            // 不许拿缓存旧句柄硬连（旧顺序先连后兜底，撞坏句柄白等一轮）。
            c.forget_device(id).await;
            let _ = c.start_scan().await;
            let fresh = poll_found_by_name(10, &target.name, || c.list_found()).await;
            let first_id = fresh.as_deref().unwrap_or(id);
            match c.connect(first_id).await {
                Ok(conn) => Ok(conn),
                Err(e) if e.to_string().to_lowercase().contains("bad exception") => {
                    // #18：bad exception = 适配器状态损坏，先重初始化再捕获重连
                    bluelink_services::log_line(
                        "simpleble",
                        "自动重连撞上 bad exception：重初始化适配器后重新捕获",
                    );
                    c.reinit().await;
                    let _ = c.start_scan().await;
                    let fresh2 = poll_found_by_name(10, &target.name, || c.list_found()).await;
                    match fresh2 {
                        Some(fid) => c.connect(&fid).await.map_err(|e| e.to_string()),
                        None => Err("重初始化后重扫未找到目标设备".to_string()),
                    }
                }
                Err(e) => Err(e.to_string()),
            }
        }
        _ => {
            let c = central.clone().ok_or("蓝牙不可用")?;
            // #19 捕获先行（同 SimpleBLE 路）：弃缓存、重扫按名捕获再连。
            c.forget_device(id).await;
            let _ = c.start_scan().await;
            let fresh = poll_found_by_name(10, &target.name, || c.list_found()).await;
            let first_id = fresh.as_deref().unwrap_or(id);
            c.connect(first_id).await.map_err(|e| e.to_string())
        }
    }
}

/// 在给定秒数内轮询扫描结果，按设备名找新鲜条目（重连兜底用）。
async fn poll_found_by_name<F, Fut>(secs: u64, name: &str, mut list: F) -> Option<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
        Output = Result<
            Vec<bluelink_transport::ble::FoundDevice>,
            bluelink_transport::TransportError,
        >,
    >,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Ok(devices) = list().await {
            if let Some(d) = devices.iter().find(|d| d.name == name) {
                return Some(d.id.clone());
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    state.node.disconnect().await;
    Ok(())
}

/// 连接尝试令牌：发起连接时领取代号；取消时把计数推进，旧代号即作废。
impl AppState {
    fn begin_connect(&self) -> u64 {
        self.connect_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    fn connect_current(&self, gen: u64) -> bool {
        self.connect_gen.load(std::sync::atomic::Ordering::SeqCst) == gen
    }
}

/// 取消正在进行的连接尝试：立即作废其令牌。底层连接（尤其经典蓝牙阻塞在
/// 系统 connect 里）可能迟到才返回，连接命令会在接管前/后核对令牌，
/// 作废的尝试自行收尾（断开），绝不进会话、不写上次连接记录。
#[tauri::command]
async fn cancel_connect(state: State<'_, AppState>) -> Result<(), String> {
    state
        .connect_gen
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    bluelink_services::log_line("app", "用户取消了正在进行的连接尝试（令牌已作废）");
    Ok(())
}

#[tauri::command]
async fn send_text(
    state: State<'_, AppState>,
    text: String,
    reply_to: Option<String>,
) -> Result<Message, String> {
    state
        .node
        .send_text(&text, reply_to)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_file_path(state: State<'_, AppState>, path: String) -> Result<String, String> {
    state
        .node
        .send_file(std::path::Path::new(&path))
        .await
        .map_err(|e| e.to_string())
}

/// 发送图片组（v2.6.0）：多张图片合并为一条消息发送，返回组消息 ID。
#[tauri::command]
async fn send_image_group(
    state: State<'_, AppState>,
    paths: Vec<String>,
) -> Result<String, String> {
    let paths: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
    state
        .node
        .send_image_group(&paths)
        .await
        .map_err(|e| e.to_string())
}

/// 图片组成员文件信息（组气泡缩略图/查看器/保存全部用），顺序与组内一致。
#[tauri::command]
fn group_files(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<Vec<FileInfoView>, String> {
    Ok(state
        .node
        .group_files(&message_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| FileInfoView {
            name: f.name,
            size: f.size,
            path: f.path,
            exists: f.exists,
        })
        .collect())
}

/// 「保存全部图片」：组内图片按序复制到目标目录（1、2、3…保留扩展名）。
/// 返回（成功数， 总数），前端据此提示。
#[tauri::command]
fn save_group_images(
    state: State<'_, AppState>,
    message_id: String,
    dir: String,
) -> Result<(usize, usize), String> {
    state
        .node
        .save_group_images(&message_id, std::path::Path::new(&dir))
        .map_err(|e| e.to_string())
}

/// 把一段文本（代码/长文）落成本地文档后按文件发送：前端负责识别类型与命名，
/// 这里消毒文件名防路径穿越，写入数据目录的 outbox 再走文件引擎发送。
#[tauri::command]
async fn send_document(
    state: State<'_, AppState>,
    name: String,
    content: String,
) -> Result<String, String> {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches('.').to_string();
    if safe.is_empty() || !safe.contains('.') {
        return Err("文件名无效".into());
    }
    let dir = state.data_dir.join("outbox");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&safe);
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    state.node.send_file(&path).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_typing(state: State<'_, AppState>, typing: bool) -> Result<(), String> {
    state.node.send_typing(typing).await;
    Ok(())
}

#[tauri::command]
fn list_conversations(state: State<'_, AppState>) -> Result<Vec<Conversation>, String> {
    state
        .node
        .storage
        .list_conversations()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_messages(
    state: State<'_, AppState>,
    conversation_id: String,
    before: Option<i64>,
    limit: Option<i64>,
) -> Result<Vec<Message>, String> {
    state
        .node
        .storage
        .list_messages(&conversation_id, before, limit.unwrap_or(200))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn search_messages(state: State<'_, AppState>, query: String) -> Result<Vec<Message>, String> {
    state
        .node
        .storage
        .search_messages(&query)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_message_tags(
    state: State<'_, AppState>,
    message_id: String,
    tags: Vec<String>,
) -> Result<(), String> {
    state
        .node
        .storage
        .set_message_tags(&message_id, &tags)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_message_tags(state: State<'_, AppState>, message_id: String) -> Result<Vec<String>, String> {
    state
        .node
        .storage
        .get_message_tags(&message_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_tags(state: State<'_, AppState>) -> Result<Vec<Tag>, String> {
    state.node.storage.list_tags().map_err(|e| e.to_string())
}

/// 设置标签颜色（调色板序号 0–8，0 为默认），供聊天页右上角彩签自定义颜色。
#[tauri::command]
fn set_tag_color(state: State<'_, AppState>, tag: String, color: i64) -> Result<(), String> {
    if !(0..=8).contains(&color) {
        return Err("颜色序号无效".into());
    }
    state
        .node
        .storage
        .set_tag_color(&tag, color)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_messages_by_tag(state: State<'_, AppState>, tag: String) -> Result<Vec<Message>, String> {
    state
        .node
        .storage
        .list_messages_by_tag(&tag)
        .map_err(|e| e.to_string())
}

fn parse_mark(kind: &str) -> Result<MarkKind, String> {
    match kind {
        "favorite" => Ok(MarkKind::Favorite),
        "pin" => Ok(MarkKind::Pin),
        _ => Err("未知标记类型".into()),
    }
}

#[tauri::command]
fn toggle_mark(
    state: State<'_, AppState>,
    message_id: String,
    kind: String,
) -> Result<bool, String> {
    state
        .node
        .storage
        .toggle_mark(&message_id, parse_mark(&kind)?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_marked(state: State<'_, AppState>, kind: String) -> Result<Vec<Message>, String> {
    state
        .node
        .storage
        .list_marked(parse_mark(&kind)?)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn save_draft(
    state: State<'_, AppState>,
    conversation_id: String,
    text: String,
) -> Result<(), String> {
    state
        .node
        .storage
        .save_draft(&conversation_id, &text)
        .map_err(|e| e.to_string())
}

/// 把文本写入指定路径（聊天记录导出用，路径由保存对话框选定）。
#[tauri::command]
fn save_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| format!("写入失败：{e}"))
}

#[tauri::command]
async fn ai_send(
    state: State<'_, AppState>,
    text: String,
    thinking: Option<bool>,
    conversation_id: Option<String>,
) -> Result<Message, String> {
    let (tx, rx) = oneshot::channel::<()>();
    *state.ai_cancel.lock().await = Some(tx);
    let node = state.node.clone();
    tokio::select! {
        r = node.ai_send(&text, thinking, conversation_id) => r.map_err(|e| e.to_string()),
        _ = rx => Err("已停止生成".to_string()),
    }
}

/// ㉕ 新建 AI 会话（AI 页「新建会话」）。
#[tauri::command]
fn create_ai_conversation(
    state: State<'_, AppState>,
    title: Option<String>,
) -> Result<Conversation, String> {
    state
        .node
        .create_ai_conversation(title)
        .map_err(|e| e.to_string())
}

/// ㉕ AI 会话改名。
#[tauri::command]
fn rename_ai_conversation(
    state: State<'_, AppState>,
    conversation_id: String,
    title: String,
) -> Result<(), String> {
    state
        .node
        .rename_ai_conversation(&conversation_id, &title)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_ai_conversation(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<(), String> {
    state
        .node
        .delete_ai_conversation(&conversation_id)
        .map_err(|e| e.to_string())
}

/// 确保默认会话「AI 助手」（ai-default）的行存在（v2.6.2 补）：它原先
/// 只在首次发 AI 消息时才落库，会话条在那之前不显它、删除回落也被
/// 顶掉。前端加载会话条时缺了就调本命令补行（INSERT OR IGNORE 幂等）。
#[tauri::command]
fn ensure_ai_conversation(state: State<'_, AppState>) -> Result<String, String> {
    state
        .node
        .ensure_ai_conversation()
        .map_err(|e| e.to_string())
}

/// 聊天页「问AI」：把指定聊天消息作为上下文发给默认 AI，问答落在 AI 会话。
#[tauri::command]
async fn ai_ask_message(
    state: State<'_, AppState>,
    conversation_id: String,
    message_id: String,
    thinking: Option<bool>,
    extra: Option<String>,
) -> Result<Message, String> {
    let (tx, rx) = oneshot::channel::<()>();
    *state.ai_cancel.lock().await = Some(tx);
    let node = state.node.clone();
    tokio::select! {
        r = node.ai_ask_message(&conversation_id, &message_id, thinking, extra) => r.map_err(|e| e.to_string()),
        _ = rx => Err("已停止生成".to_string()),
    }
}

/// 聊天框 @AI 提问：source = local / peer / default，问答落在当前聊天会话（仅本机保存）。
#[tauri::command]
async fn chat_ask_ai(
    state: State<'_, AppState>,
    conversation_id: String,
    source: String,
    text: String,
    thinking: Option<bool>,
) -> Result<Message, String> {
    state
        .node
        .chat_ask_ai(
            &conversation_id,
            bluelink_services::AiSource::parse(&source),
            &text,
            thinking,
        )
        .await
        .map_err(|e| e.to_string())
}

/// 聊天里 @AI 回答的重新生成（v2.6.1）：按回答消息 ID 定位原问题重答。
#[tauri::command]
async fn chat_regenerate_ai(
    state: State<'_, AppState>,
    message_id: String,
    thinking: Option<bool>,
) -> Result<Message, String> {
    state
        .node
        .chat_regenerate_ai(&message_id, thinking)
        .await
        .map_err(|e| e.to_string())
}

/// 读取默认 AI 来源（auto / local / peer）。
#[tauri::command]
fn get_ai_source(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state
        .node
        .storage
        .get_setting("ai_default_source")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "auto".to_string()))
}

/// 保存默认 AI 来源。
#[tauri::command]
fn set_ai_source(state: State<'_, AppState>, source: String) -> Result<(), String> {
    let v = match source.as_str() {
        "local" => "local",
        "peer" => "peer",
        _ => "auto",
    };
    state
        .node
        .storage
        .set_setting("ai_default_source", v)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn ai_regenerate(
    state: State<'_, AppState>,
    thinking: Option<bool>,
    conversation_id: Option<String>,
) -> Result<Message, String> {
    let (tx, rx) = oneshot::channel::<()>();
    *state.ai_cancel.lock().await = Some(tx);
    let node = state.node.clone();
    tokio::select! {
        r = node.ai_regenerate(thinking, conversation_id) => r.map_err(|e| e.to_string()),
        _ = rx => Err("已停止生成".to_string()),
    }
}

#[tauri::command]
async fn ai_stop(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(tx) = state.ai_cancel.lock().await.take() {
        let _ = tx.send(());
    }
    Ok(())
}

/// 从设置里组装完整 AI 配置（全字段唯一出处：命令层与启动恢复共用）。
fn load_ai_config(s: &bluelink_storage::Storage) -> AiConfig {
    let get = |k: &str| s.get_setting(k).ok().flatten().unwrap_or_default();
    AiConfig {
        base_url: get("ai_base"),
        api_key: get("ai_key"),
        model: get("ai_model"),
        stream: get("ai_stream") != "0",
        temperature: get("ai_temp")
            .parse::<f32>()
            .ok()
            .filter(|t| (0.0..=2.0).contains(t)),
        max_tokens: get("ai_max_tokens").parse::<u32>().ok().filter(|n| *n > 0),
        system_prompt: get("ai_prompt"),
    }
}

/// AI 配置回显（不回传密钥明文，只报是否已配置）。
#[derive(Serialize)]
struct AiConfigView {
    base_url: String,
    model: String,
    has_key: bool,
    /// 是否已完整配置（地址/密钥/模型齐全，与 AI 层 is_ready 同口径）：
    /// 前端据此在点「问AI」/@AI/AI 发送时先提醒未配置，不静默失败。
    configured: bool,
    stream: bool,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    system_prompt: String,
}

#[tauri::command]
fn get_ai_config(state: State<'_, AppState>) -> Result<AiConfigView, String> {
    let cfg = load_ai_config(&state.node.storage);
    Ok(AiConfigView {
        configured: cfg.is_ready(),
        has_key: !cfg.api_key.is_empty(),
        base_url: cfg.base_url,
        model: cfg.model,
        stream: cfg.stream,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
        system_prompt: cfg.system_prompt,
    })
}

// 参数即前端 IPC 契约（AI 配置全字段），逐个平铺比打包对象更贴合现有调用方式。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn set_ai_config(
    state: State<'_, AppState>,
    base_url: String,
    api_key: String,
    model: String,
    stream: bool,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    system_prompt: String,
) -> Result<(), String> {
    let s = &state.node.storage;
    let set = |k: &str, v: &str| s.set_setting(k, v).map_err(|e| e.to_string());
    set("ai_base", &base_url)?;
    // 密钥留空表示不修改
    if !api_key.is_empty() {
        set("ai_key", &api_key)?;
    }
    set("ai_model", &model)?;
    set("ai_stream", if stream { "1" } else { "0" })?;
    set(
        "ai_temp",
        &temperature.map(|t| t.to_string()).unwrap_or_default(),
    )?;
    set(
        "ai_max_tokens",
        &max_tokens.map(|n| n.to_string()).unwrap_or_default(),
    )?;
    set("ai_prompt", &system_prompt)?;
    let cfg = load_ai_config(s);
    state
        .node
        .set_ai_config(if cfg.is_ready() { Some(cfg) } else { None })
        .await;
    Ok(())
}

/// 测试连接（v2.6.1 改）：用设置表单的当前值直测，不必先保存。
/// 密钥框留空 = 沿用已保存的密钥（表单从不回显密钥明文，留空语义即不修改），
/// 其余字段（地址/模型）一律以表单为准；测通了再保存也不迟。
#[tauri::command]
async fn test_ai_connection(
    state: State<'_, AppState>,
    base_url: String,
    api_key: String,
    model: String,
) -> Result<String, String> {
    let mut cfg = load_ai_config(&state.node.storage);
    cfg.base_url = base_url.trim().to_string();
    if !api_key.trim().is_empty() {
        cfg.api_key = api_key.trim().to_string();
    }
    cfg.model = model.trim().to_string();
    let client = AiClient::new(cfg);
    let t0 = std::time::Instant::now();
    let reply = client
        .chat_once(&[ChatMessage {
            role: "user".into(),
            content: "ping".into(),
        }])
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "连接成功（延迟约 {} ms），接口回复：{}",
        t0.elapsed().as_millis(),
        reply.chars().take(50).collect::<String>()
    ))
}

/// 用系统浏览器打开网址（设置页「项目」地址点击跳转用，v2.6.1）。
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    bluelink_services::open_url(&url).map_err(|e| e.to_string())
}

/// 前端把关键错误原文写进运行日志（检查更新失败的插件原文等，
/// 界面上只展示人话版，原文留日志备查，v2.6.1）。
#[tauri::command]
fn log_frontend(msg: String) {
    let brief: String = msg.chars().take(300).collect();
    bluelink_services::log_line("ui", &brief);
}

/// 图片查看器「复制图片」（⑱）：按路径把当前显示的那张图经原生通道
/// 写进系统剪贴板（解码 + arboard 写图都在 services，阻塞活放
/// spawn_blocking）。成功返回尺寸文案供前端 toast。
#[tauri::command]
async fn copy_image_clipboard(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let node = state.node.clone();
    let p = std::path::PathBuf::from(path);
    let (w, h) = tokio::task::spawn_blocking(move || node.copy_image_blocking(&p))
        .await
        .map_err(|e| format!("复制失败：{e}"))?
        .map_err(|e| e.to_string())?;
    Ok(format!("{w}×{h}"))
}

// ---------- 彩签分类（v2.2：颜色即分类） ----------
#[tauri::command]
fn list_categories(state: State<'_, AppState>) -> Result<Vec<Category>, String> {
    state
        .node
        .storage
        .list_categories()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_category(state: State<'_, AppState>, color: i64, name: String) -> Result<(), String> {
    let name = name.trim();
    if !(0..=8).contains(&color) {
        return Err("颜色序号无效".into());
    }
    if name.is_empty() || name.chars().count() > 20 {
        return Err("分类名需为 1–20 字".into());
    }
    state
        .node
        .storage
        .rename_category(color, name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_message_category(
    state: State<'_, AppState>,
    message_id: String,
    color: Option<i64>,
    note: Option<String>,
) -> Result<(), String> {
    if let Some(c) = color {
        if !(0..=8).contains(&c) {
            return Err("颜色序号无效".into());
        }
    }
    state
        .node
        .storage
        .set_message_category(&message_id, color.map(|c| (c, note)))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_message_category(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<Option<MessageCategory>, String> {
    state
        .node
        .storage
        .get_message_category(&message_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_messages_by_category(
    state: State<'_, AppState>,
    color: i64,
) -> Result<Vec<CategorizedMessage>, String> {
    state
        .node
        .storage
        .list_messages_by_category(color)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_message(state: State<'_, AppState>, message_id: String) -> Result<Option<Message>, String> {
    state
        .node
        .storage
        .get_message(&message_id)
        .map_err(|e| e.to_string())
}

/// 删除一条消息记录（文件菜单「删除」；只删记录与本地索引，不动已落地的文件本体）。
#[tauri::command]
fn delete_message(state: State<'_, AppState>, message_id: String) -> Result<(), String> {
    state
        .node
        .storage
        .delete_message(&message_id)
        .map_err(|e| e.to_string())
}

// ---------- 传输控制 / 存储 / 文件 ----------
#[tauri::command]
async fn cancel_transfer(state: State<'_, AppState>, transfer_id: String) -> Result<(), String> {
    state.node.cancel_transfer(&transfer_id).await;
    Ok(())
}

#[tauri::command]
async fn resend_message(state: State<'_, AppState>, message_id: String) -> Result<(), String> {
    state
        .node
        .resend_message(&message_id)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct StorageStatsView {
    chat_count: i64,
    chat_bytes: i64,
    files_bytes: u64,
    files_count: u64,
    cache_bytes: u64,
    /// 当前接收文件保存目录（设置页展示，可修改）。
    download_dir: String,
    /// 运行日志所在目录（设置页展示，可打开）。
    log_dir: String,
}

#[tauri::command]
async fn storage_stats(state: State<'_, AppState>) -> Result<StorageStatsView, String> {
    // 统计要扫下载目录逐个文件取大小，是同步重活：放阻塞线程池跑，
    // 别占 Tauri 主线程（v2.5.1 修设置页开页卡顿的头号嫌疑即此）。
    let node = state.node.clone();
    let (s, log_dir, download_dir) = tokio::task::spawn_blocking(move || {
        let s = node.storage_stats();
        let log_dir = bluelink_services::log::log_path()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().to_string()))
            .unwrap_or_default();
        let download_dir = node.download_dir().to_string_lossy().to_string();
        (s, log_dir, download_dir)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(StorageStatsView {
        chat_count: s.chat_count,
        chat_bytes: s.chat_bytes,
        files_bytes: s.files_bytes,
        files_count: s.files_count,
        cache_bytes: s.cache_bytes,
        download_dir,
        log_dir,
    })
}

/// 修改日志目录（传空 = 恢复默认 <数据目录>/logs），返回生效后的目录。
/// 旧目录文件不搬不删，只切换后续写入目标；「打开日志目录」走 log_path 自然跟随。
#[tauri::command]
fn set_log_dir(state: State<'_, AppState>, path: Option<String>) -> Result<String, String> {
    let dir = path.as_deref().filter(|s| !s.trim().is_empty());
    let effective = state
        .node
        .set_log_dir(dir.map(std::path::Path::new))
        .map_err(|e| e.to_string())?;
    Ok(effective.to_string_lossy().to_string())
}

/// 系统通知开关（设置键 sys_notify，默认开）：收到对方新消息且窗口不在前台时弹通知。
#[tauri::command]
fn get_sys_notify(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state
        .node
        .storage
        .get_setting("sys_notify")
        .map(|v| v.as_deref() != Some("0"))
        .unwrap_or(true))
}

#[tauri::command]
fn set_sys_notify(state: State<'_, AppState>, on: bool) -> Result<(), String> {
    state
        .node
        .storage
        .set_setting("sys_notify", if on { "1" } else { "0" })
        .map_err(|e| e.to_string())
}

/// 修改接收文件保存目录（传空 = 恢复默认），返回生效后的目录。
#[tauri::command]
fn set_download_dir(state: State<'_, AppState>, path: Option<String>) -> Result<String, String> {
    let dir = path.as_deref().filter(|s| !s.trim().is_empty());
    let effective = state
        .node
        .set_download_dir(dir.map(std::path::Path::new))
        .map_err(|e| e.to_string())?;
    Ok(effective.to_string_lossy().to_string())
}

/// 在系统文件管理器中打开运行日志目录。
#[tauri::command]
fn open_log_dir(state: State<'_, AppState>) -> Result<(), String> {
    let _ = &state;
    let path = bluelink_services::log::log_path().ok_or("日志尚未初始化")?;
    let dir = path.parent().ok_or("日志目录不存在")?;
    bluelink_services::open_file(dir)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn clean_cache(state: State<'_, AppState>) -> Result<u64, String> {
    Ok(state.node.clean_cache().await)
}

#[tauri::command]
fn clear_chat_history(state: State<'_, AppState>, range: Option<String>) -> Result<u64, String> {
    // #14：带范围参数走按范围清理（24h/7d/30d/all），缺省 = 全部（旧调用兼容）
    match range {
        Some(r) => state
            .node
            .clear_chat_history_range(&r)
            .map_err(|e| e.to_string()),
        None => state
            .node
            // 缺省（旧调用）也走范围口径的「全部」，如实返回删除条数
            // （第三方评审采纳：旧实现硬编码返回 0，界面会误报）
            .clear_chat_history_range("all")
            .map_err(|e| e.to_string()),
    }
}

#[derive(Serialize)]
struct FileInfoView {
    name: String,
    size: u64,
    path: Option<String>,
    exists: bool,
}

#[tauri::command]
fn file_info(
    state: State<'_, AppState>,
    message_id: String,
) -> Result<Option<FileInfoView>, String> {
    Ok(state
        .node
        .file_info(&message_id)
        .map_err(|e| e.to_string())?
        .map(|f| FileInfoView {
            name: f.name,
            size: f.size,
            path: f.path,
            exists: f.exists,
        }))
}

/// 打开文件（#9 定版）：返回 "viewer" 表示应走内置查看器（文本/
/// 代码/脚本，前端据此开查看器）；"ok" 表示后端已处理（系统打开
/// 或文件夹定位，可执行类只定位、永不打开）。
#[tauri::command]
fn open_file(path: String) -> Result<String, String> {
    match bluelink_services::open_file(std::path::Path::new(&path)).map_err(|e| e.to_string())? {
        bluelink_services::OpenOutcome::Viewer => Ok("viewer".to_string()),
        bluelink_services::OpenOutcome::Done => Ok("ok".to_string()),
    }
}

/// 文本查看器的数据上限（v2.6.2 修订）：#9 定版——文本/代码大文件
/// 在查看器内降级打开（关高亮+虚拟渲染），不再以 10MB 为由回退
/// 系统打开（脚本类回退系统打开等于执行）。上限放宽到 100MB，
/// 超过才报错，且前端只许定位显示、不许系统打开。
const TEXT_VIEWER_MAX: u64 = 100 * 1024 * 1024;
/// 超过这个大小进入降级模式（关语法高亮，只保留虚拟滚动渲染）。
const TEXT_VIEWER_DEGRADE: u64 = 10 * 1024 * 1024;

#[derive(Serialize)]
struct TextFileView {
    size: u64,
    base64: String,
    /// 降级模式（大文件关高亮），前端据此跳过 tokenizer。
    degraded: bool,
}

/// 文本查看器数据源（v2.6.0）：读文件原始字节回前端（base64）。
/// 编码识别与解码由前端 TextDecoder 做（UTF-8 严格校验，失败回退
/// GBK）——WebView 自带解码器，后端零新增依赖、零体积增量。
#[tauri::command]
fn read_text_file(path: String) -> Result<TextFileView, String> {
    let meta = std::fs::metadata(&path).map_err(|e| format!("文件不存在或不可读（{e}）"))?;
    if !meta.is_file() {
        return Err("不是文件".into());
    }
    if meta.len() > TEXT_VIEWER_MAX {
        return Err(format!("TOO_LARGE:{}", meta.len()));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("读取失败（{e}）"))?;
    Ok(TextFileView {
        size: bytes.len() as u64,
        degraded: meta.len() > TEXT_VIEWER_DEGRADE,
        base64: base64_encode(&bytes),
    })
}

/// 把一个本地文件登记进 asset 协议可见范围（v2.6.0 查看器用）：
/// 图片/音视频经 convertFileSrc 直读本地文件，不经内存拷贝。
/// 协议范围默认不开大口子，这里按文件逐个放行。
#[tauri::command]
fn allow_asset_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri::Manager as _;
    app.asset_protocol_scope()
        .allow_file(std::path::Path::new(&path))
        .map_err(|e| e.to_string())
}

/// 标准 base64 编码（与既有 base64_decode 对称，手写避免新增依赖）。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[tauri::command]
fn reveal_file(path: String) -> Result<(), String> {
    bluelink_services::reveal_file(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

/// 把文件本体放进系统剪贴板（文件菜单「复制文件」）：Windows 写
/// CF_HDROP（等同资源管理器 Ctrl+C，粘到文件夹里出来的是文件本身，
/// 用户 2026-11-20 要求，路径回退在 Windows 作废）；其他平台返回
/// UNSUPPORTED 错误，前端维持原有回退（见 win_clipboard 模块说明）。
#[tauri::command]
fn copy_file_to_clipboard(path: String) -> Result<(), String> {
    win_clipboard::set_file_list(&[path.as_str()])
}

/// 读系统剪贴板里的文件列表（输入框粘贴文件时取真实路径）：在文件
/// 管理器里复制文件后剪贴板里就是文件对象格式。Ok(空)=没有文件对象；
/// UNSUPPORTED 错误=此平台暂未实现读取（见 win_clipboard 模块说明）。
#[tauri::command]
fn clipboard_file_list() -> Result<Vec<String>, String> {
    win_clipboard::get_file_list()
}

/// 待发附件的入队校验信息：文件须存在且是普通文件，返回文件名与大小。
#[derive(Serialize)]
struct AttachInfo {
    name: String,
    size: u64,
}

#[tauri::command]
fn file_attach_info(path: String) -> Result<AttachInfo, String> {
    let p = std::path::Path::new(&path);
    let meta = std::fs::metadata(p).map_err(|e| format!("文件不存在或不可读（{e}）"))?;
    if !meta.is_file() {
        return Err("不是文件（文件夹不能直接发送）".to_string());
    }
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| "文件名无效".to_string())?;
    Ok(AttachInfo {
        name,
        size: meta.len(),
    })
}

/// 复制文件到目标路径（文件菜单「另存为」，目标由保存对话框选定）。
#[tauri::command]
fn copy_file_as(src: String, dst: String) -> Result<(), String> {
    std::fs::copy(&src, &dst)
        .map(|_| ())
        .map_err(|e| format!("复制失败：{e}"))
}

/// 把超大段文字落盘暂存为文档文件，返回路径（v2.6.5，#2）：粘贴/发送
/// 遇到几十万行量级时，文字绝不进输入框、绝不当聊天正文发——前端在
/// 插入前采样判量级，超限直接调这里转成文件入待发队列。文件名由前端
/// 按采样分类给（代码后缀或 长文.txt），这里只做基本清洗。
#[tauri::command]
fn stage_text_document(
    state: State<'_, AppState>,
    name: String,
    content: String,
) -> Result<String, String> {
    let name: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    // 清洗后为空、或只剩 "." / ".." 这类相对路径名时回落默认名
    // （第三方评审采纳：dir.join(".") 会把暂存目录本身当写入目标）
    let trimmed = name.trim();
    let name = if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "长文.txt".to_string()
    } else {
        name
    };
    if content.len() > 64 * 1024 * 1024 {
        return Err("内容过大，无法转为文件".into());
    }
    let dir = state.data_dir.join("pasted");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // 文件名就用分类名（收方看到的也是这个名）；同名冲突时加序号后缀
    let mut path = dir.join(&name);
    if path.exists() {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        for n in 2..1000 {
            let candidate = dir.join(format!("{stem} ({n}){ext}"));
            if !candidate.exists() {
                path = candidate;
                break;
            }
        }
    }
    std::fs::write(&path, content).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// 把粘贴的图片落盘暂存，返回路径（前端在点发送时按文件发送；剪贴板图片只在无文字时走这里）。
#[tauri::command]
fn stage_pasted_image(
    state: State<'_, AppState>,
    data_base64: String,
    ext: String,
) -> Result<String, String> {
    let ext = match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        _ => "png",
    };
    let bytes = base64_decode(&data_base64).ok_or("图片数据无效")?;
    if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
        return Err("图片数据无效或过大".into());
    }
    let dir = state.data_dir.join("pasted");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = format!("粘贴的图片-{}.{}", bluelink_domain::now_ms(), ext);
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// 极简 Base64 解码（标准字母表，容忍空白与缺省填充）。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut vals = [0u8; 4];
        let mut n = 0;
        let mut pad = 0;
        for (i, c) in chunk.iter().enumerate() {
            if *c == b'=' {
                pad += 1;
                vals[i] = 0;
            } else {
                vals[i] = val(*c)?;
                n += 1;
            }
        }
        if n + pad != 4 && chunk.len() == 4 {
            return None;
        }
        out.push((vals[0] << 2) | (vals[1] >> 4));
        if pad < 2 && chunk.len() > 2 {
            out.push((vals[1] << 4) | (vals[2] >> 2));
        }
        if pad < 1 && chunk.len() > 3 {
            out.push((vals[2] << 6) | vals[3]);
        }
        if chunk.len() < 4 {
            break;
        }
    }
    Some(out)
}

// ---------- AI 网关（转发许可 + 本地 API 服务） ----------
#[derive(Serialize)]
struct GatewaySettingsView {
    proxy_allow: bool,
    proxy_count: u64,
    proxy_tokens: u64,
    local_enabled: bool,
    local_running: bool,
    local_addr: String,
    local_has_key: bool,
    /// ㉙ 可用模型提示的数据面：model 取网关现成的 gateway_model()
    /// （本机配了是真实模型名，未配是占位名 bluelink-proxy），
    /// connected 是当前连接状态，前端据此区分三态口径。
    model: String,
    connected: bool,
}

fn gateway_view(state: &AppState) -> GatewaySettingsView {
    let s = &state.node.storage;
    let get = |k: &str| s.get_setting(k).ok().flatten().unwrap_or_default();
    let (count, tokens) = state.node.proxy_today_stats();
    GatewaySettingsView {
        proxy_allow: get("ai_proxy_allow") == "1",
        proxy_count: count,
        proxy_tokens: tokens,
        local_enabled: get("local_api_enabled") == "1",
        local_running: false, // 由命令层按任务句柄补齐
        local_addr: format!("http://127.0.0.1:{}/v1", local_api::PORT),
        local_has_key: !get("local_api_key").is_empty(),
        model: String::new(), // 由命令层异步补齐（gateway_model 是异步的）
        connected: false,
    }
}

#[tauri::command]
async fn get_gateway_settings(state: State<'_, AppState>) -> Result<GatewaySettingsView, String> {
    let mut v = gateway_view(&state);
    v.local_running = local_api::is_running();
    v.model = state.node.gateway_model().await;
    v.connected = state.node.is_connected().await;
    Ok(v)
}

/// 剪贴板同步状态（设置页展示）：本机开关、对方开关、是否已连接。
#[derive(Serialize)]
struct ClipboardSyncView {
    local: bool,
    peer: bool,
    connected: bool,
}

#[tauri::command]
async fn get_clipboard_sync(state: State<'_, AppState>) -> Result<ClipboardSyncView, String> {
    let (local, peer, connected) = state.node.clipboard_status().await;
    Ok(ClipboardSyncView {
        local,
        peer,
        connected,
    })
}

#[tauri::command]
async fn set_clipboard_sync(state: State<'_, AppState>, on: bool) -> Result<(), String> {
    state
        .node
        .set_clipboard_sync(on)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_proxy_allow(state: State<'_, AppState>, on: bool) -> Result<(), String> {
    state
        .node
        .storage
        .set_setting("ai_proxy_allow", if on { "1" } else { "0" })
        .map_err(|e| e.to_string())
}

/// 开关本地 API 服务；key 为 Some 时更新本机访问密钥（空串 = 清除，None = 不修改）。
#[tauri::command]
async fn set_local_api(
    state: State<'_, AppState>,
    enabled: bool,
    key: Option<String>,
) -> Result<(), String> {
    let s = &state.node.storage;
    s.set_setting("local_api_enabled", if enabled { "1" } else { "0" })
        .map_err(|e| e.to_string())?;
    if let Some(k) = key {
        s.set_setting("local_api_key", k.trim())
            .map_err(|e| e.to_string())?;
    }
    let mut slot = state.local_api.lock().await;
    if let Some(h) = slot.take() {
        h.abort();
        local_api::mark_stopped();
    }
    if enabled {
        *slot = Some(local_api::spawn(state.node.clone()));
    }
    Ok(())
}

/// 软件更新安装完成后的重启（v2.5.2，前端 updater 流程专用命令）。
/// Linux 以 AppImage 运行时：updater 已把 $APPIMAGE 文件替换为新版，必须按
/// AppImage 路径重新拉起（进程内 current_exe 指向挂载/解压的旧副本路径，
/// 直接 restart 会启到旧版或失效路径）；其余情况走 Tauri 标准 restart。
#[tauri::command]
fn restart_app(app: AppHandle) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(img) = std::env::var("APPIMAGE") {
            if !img.is_empty() {
                let _ = std::process::Command::new(&img)
                    .arg("--appimage-extract-and-run")
                    .spawn();
                app.exit(0);
                return;
            }
        }
    }
    app.restart();
}

/// 原生窗口标题栏主题跟随应用主题（v2.5.2）：Windows 用原生标题栏，
/// 应用切深色时标题栏默认仍是白色，需经此命令同步（Window::set_theme
/// 需 tauri 的 unstable 特性）。"dark" → 深色、"light" → 浅色、
/// "system" 或其他值 → None（跟随系统）。失败静默（个别平台不支持）。
#[tauri::command]
fn set_window_theme(window: tauri::Window, theme: String) {
    let t = match theme.as_str() {
        "dark" => Some(tauri::Theme::Dark),
        "light" => Some(tauri::Theme::Light),
        _ => None,
    };
    let _ = window.set_theme(t);
}

/// 通知被点击后的统一处理（v2.5.2）：把主窗口带回前台（托盘隐藏先恢复
/// 显示、最小化先还原），并通知前端打开对应会话。Windows 由 WinRT 激活
/// 回调调用（win_notify 模块）；其他平台暂无点击来源（官方通知插件在
/// 桌面端没有点击回调）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn on_notification_activated(app: &AppHandle, conversation_id: &str) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    let _ = app.emit(
        "notification_activated",
        serde_json::json!({ "conversation_id": conversation_id }),
    );
}

/// 系统通知（v2.5.1）：收到对方新聊天消息时，仅当开关开且窗口不在前台
/// （隐藏到托盘/最小化/失焦）才弹。窗口可见且有焦点 = 用户正看着应用，不弹。
/// 只针对聊天新消息；连接/AI 等其他事件不弹。
/// 发送通道（v2.5.2）：Windows 走原生 WinRT toast 并带点击激活回调（点通知
/// 回窗并打开会话，见 win_notify）；其他平台仍走 tauri-plugin-notification
/// （桌面端无点击回调，那些平台点通知回窗未实现，如实记录，见 QA）。
fn maybe_notify_new_message(app: &AppHandle, node: &Arc<Node>, m: &Message) {
    use bluelink_domain::{MessageKind, Sender};
    if m.sender != Sender::Peer || m.kind == MessageKind::System {
        return;
    }
    let on = node
        .storage
        .get_setting("sys_notify")
        .map(|v| v.as_deref() != Some("0"))
        .unwrap_or(true);
    if !on {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let visible = w.is_visible().unwrap_or(false);
        let focused = w.is_focused().unwrap_or(false);
        if visible && focused {
            return; // 正看着应用，不自扰
        }
    }
    let title = node
        .storage
        .list_conversations()
        .ok()
        .and_then(|cs| {
            cs.into_iter()
                .find(|c| c.id == m.conversation_id)
                .map(|c| c.title)
        })
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "对方".to_string());
    let body = if m.kind == MessageKind::File {
        let lower = m.body.to_ascii_lowercase();
        if [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"]
            .iter()
            .any(|e| lower.ends_with(e))
        {
            format!("[图片] {}", m.body)
        } else {
            format!("[文件] {}", m.body)
        }
    } else {
        let excerpt: String = m.body.chars().take(60).collect();
        if m.body.chars().count() > 60 {
            format!("{excerpt}…")
        } else {
            excerpt
        }
    };
    #[cfg(windows)]
    win_notify::send(&title, &body, &m.conversation_id);
    #[cfg(not(windows))]
    {
        use tauri_plugin_notification::NotificationExt as _;
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

/// 把节点事件转发给前端（事件名与详细设计第三章一致）。
fn forward_events(app: AppHandle, node: Arc<Node>) {
    let mut rx = node.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::ConnectionChanged {
                    connected,
                    peer_name,
                    peer_device_id,
                    conversation_id,
                }) => {
                    let _ = app.emit(
                        "connection_state",
                        serde_json::json!({"connected": connected, "peer_name": peer_name, "peer_device_id": peer_device_id, "conversation_id": conversation_id}),
                    );
                }
                Ok(Event::MessageNew(m)) => {
                    maybe_notify_new_message(&app, &node, &m);
                    let _ = app.emit("message_new", m);
                }
                Ok(Event::MessageUpdated(m)) => {
                    let _ = app.emit("message_updated", m);
                }
                Ok(Event::MessageStatus { id, status }) => {
                    let _ = app.emit(
                        "message_status",
                        serde_json::json!({"id": id, "status": status}),
                    );
                }
                Ok(Event::TransferProgress {
                    transfer_id,
                    msg_id,
                    received,
                    total,
                }) => {
                    let _ = app.emit("transfer_progress", serde_json::json!({"transfer_id": transfer_id, "msg_id": msg_id, "received": received, "total": total}));
                }
                Ok(Event::TransferState { transfer_id, state }) => {
                    let _ = app.emit(
                        "transfer_state",
                        serde_json::json!({"transfer_id": transfer_id, "state": state}),
                    );
                }
                Ok(Event::AiChunk {
                    conversation_id,
                    delta,
                }) => {
                    let _ = app.emit(
                        "ai_chunk",
                        serde_json::json!({"conversation_id": conversation_id, "delta": delta}),
                    );
                }
                Ok(Event::AiDone {
                    conversation_id,
                    message,
                }) => {
                    let _ = app.emit(
                        "ai_done",
                        serde_json::json!({"conversation_id": conversation_id, "message": message}),
                    );
                }
                Ok(Event::AiError {
                    conversation_id,
                    error,
                }) => {
                    let _ = app.emit(
                        "ai_error",
                        serde_json::json!({"conversation_id": conversation_id, "error": error}),
                    );
                }
                Ok(Event::AiThinkingNote) => {
                    let _ = app.emit("ai_thinking_note", serde_json::json!({}));
                }
                Ok(Event::PeerTyping(t)) => {
                    let _ = app.emit("peer_typing", t);
                }
                Ok(Event::ClipboardState {
                    local,
                    peer,
                    connected,
                }) => {
                    let _ = app.emit(
                        "clipboard_state",
                        serde_json::json!({"local": local, "peer": peer, "connected": connected}),
                    );
                }
                Ok(Event::ClipboardSynced { kind }) => {
                    let _ = app.emit("clipboard_synced", serde_json::json!({"kind": kind}));
                }
                Ok(Event::ReconnectState {
                    peer_name,
                    attempt,
                    state,
                }) => {
                    let _ = app.emit(
                        "reconnect_state",
                        serde_json::json!({"peer_name": peer_name, "attempt": attempt, "state": state}),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

/// ㉗ 按主显示器工作区把窗口居中并钳制在工作区内（见 setup 调用点）。
/// 窗口物理尺寸不读 outer_size——setup 时机上窗口可能还没完成首帧
/// 尺寸协商（Linux 实测读到 0×0，居中会算成左上角落屏幕正中）；用
/// 配置里的逻辑尺寸 × 显示器缩放比换算，与实际尺寸恒等（窗口尺寸
/// 固定、最小尺寸同值）。
fn center_window_clamped(win: &tauri::WebviewWindow, logical_w: f64, logical_h: f64) {
    let Ok(Some(monitor)) = win.primary_monitor() else {
        return;
    };
    let wa = monitor.work_area();
    let scale = monitor.scale_factor();
    let pw = (logical_w * scale) as i32;
    let ph = (logical_h * scale) as i32;
    let x = wa.position.x + ((wa.size.width as i32 - pw) / 2).max(0);
    let y = wa.position.y + ((wa.size.height as i32 - ph) / 2).max(0);
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

/// ㉘ 前端主界面就绪（init 走完）后调用：窗口在 setup 里已随外壳显示，
/// 这里只做聚焦确认（幂等）。托盘恢复走托盘自己的 show，不经这里、
/// 更不会重新居中。
#[tauri::command]
fn ui_ready(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        if !win.is_visible().unwrap_or(false) {
            win.show().map_err(|e| e.to_string())?;
        }
        let _ = win.set_focus();
    }
    Ok(())
}

fn main() {
    // 传输层诊断日志（连接分步日志等）接上运行日志通道
    bluelink_transport::set_log_sink(bluelink_services::log_line);
    bluelink_transport::set_step_sink(bluelink_services::log::log_verbose);
    tauri::Builder::default()
        // 单实例必须最先注册：第二次启动直接退出，只把已有窗口唤到前台，
        // 避免双开进程抢占经典蓝牙服务端端口（WSA 10048 的主要来源之一）。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager as _;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            // ㉗ 启动居中（只在进程启动这一次）：按主屏工作区实测现算，
            // 禁止写死坐标；工作区比窗口小时钳回左上角，保标题栏可见、
            // 绝不落负坐标。托盘恢复不走这里（保持原位）。
            // ㉘ 居中后立刻显示：窗口底色已在配置里设成应用底色，页面一
            // 解析先画启动外壳（品牌+加载指示），主界面就绪后由前端撤下
            // 外壳——全程没有纯白屏，外壳也不设最短停留。
            if let Some(win) = app.get_webview_window("main") {
                let (lw, lh) = app
                    .config()
                    .app
                    .windows
                    .first()
                    .map(|w| (w.width, w.height))
                    .unwrap_or((900.0, 600.0));
                center_window_clamped(&win, lw, lh);
                let _ = win.show();
            }
            // Windows 原生通知（WinRT toast + 点击激活回调）尽早初始化：
            // COM 激活类注册要赶在系统可能投递激活请求之前完成。
            #[cfg(windows)]
            win_notify::init(app.handle().clone());
            let data_dir = app.path().app_data_dir()?;
            let name = std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "我的电脑".to_string());
            let node = Node::new(&data_dir, &name).map_err(|e| e.to_string())?;
            // 恢复已保存的 AI 配置（全字段）
            {
                let cfg = load_ai_config(&node.storage);
                if cfg.is_ready() {
                    let n = node.clone();
                    tauri::async_runtime::block_on(async move { n.set_ai_config(Some(cfg)).await });
                }
            }
            // 本地 API 服务按设置自启
            let local_handle = if node
                .storage
                .get_setting("local_api_enabled")
                .map(|v| v.as_deref() == Some("1"))
                .unwrap_or(false)
            {
                Some(local_api::spawn(node.clone()))
            } else {
                None
            };
            forward_events(app.handle().clone(), node.clone());
            // 已保存的蓝牙传输选择先读出来：内置 BLE 与 SimpleBLE 同为 BLE
            // 外设角色（都要为同一个 BlueLink 服务注册 GATT 广播），启动时
            // 选了 SimpleBLE 就只起 SimpleBLE 服务端——两个广播器同启抢同一
            // 服务 UUID，真机实证两台机器必现 SimpleBLE 广播启动失败
            // （「bad exception」，同进程的内置 BLE 广播却正常）。经典蓝牙
            // 服务端走 RFCOMM、与 BLE 广播无关，既定并行设计不动。
            let sble_compiled = bluelink_transport::simpleble::simpleble_compiled();
            let saved_bt_transport = node
                .storage
                .get_setting("bt_transport")
                .ok()
                .flatten()
                .unwrap_or_else(|| "classic".to_string());
            let want_sble_server = sble_compiled && saved_bt_transport == "simpleble";
            // 蓝牙运行时：中心端（扫描/连出）+ 服务端（广播/被连入）。
            // 无适配器或未开启时为 None，应用照常启动，界面如实提示（bt_state）。
            let (central, server, server_error, server_note) =
                tauri::async_runtime::block_on(async {
                // ㉘ 中心端与服务端互不依赖，并行初始化（旧版串行 await，
                // 启动墙钟是两者之和；真机上 GATT 注册并不便宜）
                let central_fut = async {
                    bluelink_transport::ble::ble_central()
                        .await
                        .map(Arc::new)
                        .map_err(|e| {
                            bluelink_services::log_line("ble", &format!("蓝牙中心端不可用：{e}"));
                            e
                        })
                        .ok()
                };
                let server_fut = async {
                let mut server_error = None;
                let mut server_note = None;
                let server = if want_sble_server {
                    // 与 SimpleBLE 广播互斥：本进程只起 SimpleBLE 那套服务端
                    bluelink_services::log_line(
                        "ble",
                        "启动时选择了 SimpleBLE：跳过内置 BLE 服务端启动，避免两个 BLE 广播器同启冲突（切换回 BLE 后重启生效）",
                    );
                    server_note = Some(
                        "启动时选择了 SimpleBLE，内置 BLE 广播未启动（与 SimpleBLE 广播互斥）；切换回 BLE 后重启应用生效"
                            .to_string(),
                    );
                    None
                } else {
                    match bluelink_transport::ble::start_ble_server(&name).await {
                    Ok((srv, mut inbound)) => {
                        let node_in = node.clone();
                        tauri::async_runtime::spawn(async move {
                            while let Some(conn) = inbound.recv().await {
                                // 宿主侧握手：校验对方给出的连接码是否为本机连接码
                                if let Err(e) = node_in.attach(conn, true, "").await {
                                    bluelink_services::log_line(
                                        "ble",
                                        &format!("蓝牙入站连接握手失败：{e}"),
                                    );
                                }
                            }
                        });
                        Some(Arc::new(srv))
                    }
                    Err(e) => {
                        bluelink_services::log_line(
                            "ble",
                            &format!("蓝牙服务端（广播）启动失败：{e}"),
                        );
                        server_error = Some(e.to_string());
                        None
                    }
                    }
                };
                (server, server_error, server_note)
                };
                let (central, (server, server_error, server_note)) =
                    tokio::join!(central_fut, server_fut);
                (central, server, server_error, server_note)
            });
            // 经典蓝牙运行时（仅 Windows）：Inquiry 中心端 + RFCOMM 服务端（SDP 注册 SPP）。
            // 与 BLE 服务端并行存在：哪个通道连进来就走哪个，入站一律走服务层宿主握手。
            // 非 Windows 平台 classic_supported() 为 false，三个字段恒为 None，界面不显示经典选项。
            let (classic_central, classic_server, classic_server_error) =
                if bluelink_transport::classic::classic_supported() {
                    let central = bluelink_transport::classic::classic_central()
                        .map(Arc::new)
                        .map_err(|e| {
                            bluelink_services::log_line(
                                "classic",
                                &format!("经典蓝牙中心端不可用：{e}"),
                            );
                            e
                        })
                        .ok();
                    let mut classic_error = None;
                    let server = match bluelink_transport::classic::start_classic_server() {
                        Ok((srv, mut inbound)) => {
                            let node_in = node.clone();
                            tauri::async_runtime::spawn(async move {
                                while let Some(conn) = inbound.recv().await {
                                    // 宿主侧握手：与 BLE 入站同一套校验
                                    if let Err(e) = node_in.attach(conn, true, "").await {
                                        bluelink_services::log_line(
                                            "classic",
                                            &format!("经典蓝牙入站连接握手失败：{e}"),
                                        );
                                    }
                                }
                            });
                            Some(Arc::new(srv))
                        }
                        Err(e) => {
                            bluelink_services::log_line(
                                "classic",
                                &format!("经典蓝牙服务端启动失败：{e}"),
                            );
                            classic_error = Some(e.to_string());
                            None
                        }
                    };
                    (central, server, classic_error)
                } else {
                    (None, None, None)
                };
            // SimpleBLE 运行时（第三个蓝牙实现，开发者模式可选）：
            // - 未编译（feature 未开启）：三个字段恒为 None；若设置里选了 simpleble，
            //   在日志里如实写明并回落（resolve_transport 在扫描/连接时执行回落）。
            // - 已编译：中心端照常创建（扫描/连出随时可用）；服务端只在启动时
            //   设置已选 simpleble 才拉起，避免与 BLE 服务端同时抢同一适配器的
            //   外设角色——切换传输后需重启应用，本机才经 SimpleBLE 被发现。
            if saved_bt_transport == "simpleble" && !sble_compiled {
                bluelink_services::log_line(
                    "simpleble",
                    "设置选择了 SimpleBLE，但此版本未编译 SimpleBLE（构建时未开启 simpleble feature），已回落经典蓝牙",
                );
            }
            let (sble_central, sble_server, sble_server_error, sble_central_error) =
                if sble_compiled {
                    tauri::async_runtime::block_on(async {
                        let mut central_error = None;
                        let central = match bluelink_transport::simpleble::sble_central().await {
                            Ok(c) => Some(Arc::new(c)),
                            Err(e) => {
                                bluelink_services::log_line(
                                    "simpleble",
                                    &format!("SimpleBLE 中心端不可用：{e}"),
                                );
                                central_error = Some(e.to_string());
                                None
                            }
                        };
                        let mut sble_error = None;
                        let server = if saved_bt_transport == "simpleble" {
                            // 广播启动带重试（3 次、间隔递增）：真机实证过启动即抛
                            // 库异常（bad exception），一次失败就躺平不合理；
                            // 每次尝试的结果都记日志，下次出问题一看便知。
                            let mut attempt = 0u32;
                            loop {
                                attempt += 1;
                                match bluelink_transport::simpleble::start_sble_server(&name)
                                    .await
                                {
                                    Ok((srv, mut inbound)) => {
                                        if attempt > 1 {
                                            bluelink_services::log_line(
                                                "simpleble",
                                                &format!(
                                                    "SimpleBLE 服务端（广播）启动成功（第 {attempt} 次尝试）"
                                                ),
                                            );
                                        }
                                        let node_in = node.clone();
                                        tauri::async_runtime::spawn(async move {
                                            while let Some(conn) = inbound.recv().await {
                                                // 宿主侧握手：与 BLE/经典入站同一套校验
                                                if let Err(e) =
                                                    node_in.attach(conn, true, "").await
                                                {
                                                    bluelink_services::log_line(
                                                        "simpleble",
                                                        &format!(
                                                            "SimpleBLE 入站连接握手失败：{e}"
                                                        ),
                                                    );
                                                }
                                            }
                                        });
                                        break Some(Arc::new(srv));
                                    }
                                    Err(e) => {
                                        bluelink_services::log_line(
                                            "simpleble",
                                            &format!(
                                                "SimpleBLE 服务端（广播）启动失败（第 {attempt} 次）：{e}"
                                            ),
                                        );
                                        if attempt >= 3 {
                                            sble_error = Some(e.to_string());
                                            break None;
                                        }
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            if attempt == 1 { 800 } else { 2000 },
                                        ))
                                        .await;
                                    }
                                }
                            }
                        } else {
                            None
                        };
                        (central, server, sble_error, central_error)
                    })
                } else {
                    (None, None, None, None)
                };
            // ㉘ 兜底：前端若因异常没调 ui_ready，8 秒后也把窗口亮出来，
            // 绝不让用户对着一个看不见的进程干等。
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                    if let Some(win) = handle.get_webview_window("main") {
                        if !win.is_visible().unwrap_or(true) {
                            let _ = win.show();
                        }
                    }
                });
            }
            // 自动重连拨号器注册（v2.6.2）：服务层在意外断线时按
            // 路线回调这里重新拨号（蓝牙走各中心端、局域网走 TCP）。
            {
                let dialer = make_reconnect_dialer(
                    central.clone(),
                    classic_central.clone(),
                    sble_central.clone(),
                );
                let node_reg = node.clone();
                tauri::async_runtime::spawn(async move {
                    node_reg.set_reconnect_dialer(dialer).await;
                });
            }
            app.manage(AppState {
                node,
                data_dir,
                ai_cancel: Mutex::new(None),
                central,
                server,
                server_error,
                server_note,
                connect_gen: std::sync::atomic::AtomicU64::new(0),
                classic_central,
                classic_server,
                classic_server_error,
                sble_central,
                sble_server,
                sble_server_error,
                sble_central_error,
                scan_task: Mutex::new(None),
                listening: Mutex::new(None),
                local_api: Mutex::new(local_handle),
            });
            // 系统托盘：聊天软件的标准行为——关窗口只隐藏到托盘，蓝牙与连接不断；
            // 真正退出走托盘菜单「退出」。左键点托盘图标重新打开窗口。
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
                use tauri::Manager as _;
                let show_item =
                    MenuItem::with_id(app, "tray_show", "打开 BlueLink", true, None::<&str>)?;
                let quit_item = MenuItem::with_id(app, "tray_quit", "退出", true, None::<&str>)?;
                let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;
                let mut tray_builder = TrayIconBuilder::new()
                    .menu(&tray_menu)
                    .show_menu_on_left_click(false)
                    .tooltip("BlueLink")
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "tray_show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "tray_quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(w) = app.get_webview_window("main") {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray_builder = tray_builder.icon(icon.clone());
                }
                // Linux 上缺 appindicator 运行库时托盘构建会直接 panic（第三方库行为），
                // 先查 ld 缓存确认库存在；缺失则跳过托盘并记日志，绝不让托盘拖垮启动。
                #[cfg(target_os = "linux")]
                let tray_supported = {
                    let in_cache = std::fs::read("/etc/ld.so.cache")
                        .map(|cache| {
                            let text = String::from_utf8_lossy(&cache);
                            text.contains("libayatana-appindicator3.so")
                                || text.contains("libappindicator3.so")
                        })
                        .unwrap_or(false);
                    // AppImage 等场景库在 LD_LIBRARY_PATH 的自带目录里，ld 缓存查不到，一并检查
                    let in_ld_path = std::env::var("LD_LIBRARY_PATH")
                        .map(|paths| {
                            paths.split(':').filter(|d| !d.is_empty()).any(|dir| {
                                ["libayatana-appindicator3.so.1", "libappindicator3.so.1"]
                                    .iter()
                                    .any(|f| std::path::Path::new(dir).join(f).exists())
                            })
                        })
                        .unwrap_or(false);
                    in_cache || in_ld_path
                };
                #[cfg(not(target_os = "linux"))]
                let tray_supported = true;
                let mut tray_ok = false;
                if tray_supported {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tray_builder.build(app)
                    })) {
                        Ok(Ok(_tray)) => tray_ok = true,
                        Ok(Err(e)) => bluelink_services::log_line(
                            "app",
                            &format!("系统托盘创建失败，已跳过：{e}"),
                        ),
                        Err(_) => bluelink_services::log_line(
                            "app",
                            "系统托盘创建异常，已跳过（不影响使用）",
                        ),
                    }
                } else {
                    bluelink_services::log_line(
                        "app",
                        "系统缺少 appindicator 运行库，已跳过系统托盘（关窗口将直接退出）",
                    );
                }
                // 只有托盘真建好了才拦截关闭；没托盘时关窗口就是退出，不能把窗口藏到找不回来
                if tray_ok {
                    if let Some(window) = app.get_webview_window("main") {
                        let win = window.clone();
                        window.on_window_event(move |event| {
                            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                                api.prevent_close();
                                let _ = win.hide();
                                bluelink_services::log_line(
                                    "app",
                                    "窗口关闭：已隐藏到系统托盘（应用继续运行）",
                                );
                            }
                        });
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_identity,
            bt_state,
            bt_start_scan,
            bt_stop_scan,
            bt_connect,
            get_dev_mode,
            set_dev_mode,
            get_bt_transport,
            set_bt_transport,
            start_listening,
            connect_to,
            cancel_connect,
            get_last_peer,
            set_last_peer,
            disconnect,
            send_text,
            send_file_path,
            send_image_group,
            send_document,
            send_typing,
            list_conversations,
            list_messages,
            search_messages,
            set_message_tags,
            get_message_tags,
            list_tags,
            set_tag_color,
            list_messages_by_tag,
            toggle_mark,
            list_marked,
            save_draft,
            save_text_file,
            ai_send,
            create_ai_conversation,
            rename_ai_conversation,
            delete_ai_conversation,
            ensure_ai_conversation,
            ai_ask_message,
            chat_ask_ai,
            chat_regenerate_ai,
            get_ai_source,
            set_ai_source,
            ai_regenerate,
            ai_stop,
            get_ai_config,
            set_ai_config,
            test_ai_connection,
            open_external_url,
            log_frontend,
            copy_image_clipboard,
            ui_ready,
            list_categories,
            rename_category,
            set_message_category,
            get_message_category,
            list_messages_by_category,
            get_message,
            delete_message,
            cancel_transfer,
            resend_message,
            storage_stats,
            set_download_dir,
            open_log_dir,
            set_log_dir,
            get_sys_notify,
            set_sys_notify,
            restart_app,
            set_window_theme,
            clean_cache,
            clear_chat_history,
            file_info,
            group_files,
            save_group_images,
            read_text_file,
            allow_asset_file,
            open_file,
            reveal_file,
            copy_file_as,
            copy_file_to_clipboard,
            clipboard_file_list,
            file_attach_info,
            stage_pasted_image,
            stage_text_document,
            get_gateway_settings,
            set_proxy_allow,
            set_local_api,
            get_clipboard_sync,
            set_clipboard_sync,
        ])
        .run(tauri::generate_context!())
        .expect("启动 BlueLink 失败");
}
