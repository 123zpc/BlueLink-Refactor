// BlueLink 桌面应用（Tauri 2）：命令层把前端调用转给服务层节点，
// 事件层把节点事件转发给前端。命令清单与详细设计第三章对应。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod local_api;

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
    scan_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    listening: Mutex<Option<tokio::task::JoinHandle<()>>>,
    local_api: Mutex<Option<tokio::task::JoinHandle<()>>>,
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

/// 蓝牙状态（连接页展示）：适配器是否可用、广播（可被发现）是否已开启。
#[derive(Serialize)]
struct BtState {
    available: bool,
    advertising: bool,
    message: String,
}

#[tauri::command]
async fn bt_state(state: State<'_, AppState>) -> Result<BtState, String> {
    let available = state.central.is_some();
    let advertising = match &state.server {
        Some(s) => s.is_powered().await,
        None => false,
    };
    let message = if !available {
        "未检测到蓝牙适配器或蓝牙未开启".to_string()
    } else if advertising {
        "蓝牙已开启，本机可被附近设备发现".to_string()
    } else {
        "蓝牙已开启，但本机广播未启动（系统限制）".to_string()
    };
    Ok(BtState {
        available,
        advertising,
        message,
    })
}

/// 开始扫描附近的 BlueLink 设备：结果经 bt_devices 事件每秒推送快照。
#[tauri::command]
async fn bt_start_scan(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    let central = state
        .central
        .clone()
        .ok_or_else(|| "蓝牙不可用".to_string())?;
    central.start_scan().await.map_err(|e| e.to_string())?;
    if let Some(h) = state.scan_task.lock().await.take() {
        h.abort();
    }
    let handle = tokio::spawn(async move {
        loop {
            match central.list_found().await {
                Ok(devices) => {
                    let _ = app.emit("bt_devices", devices);
                }
                Err(_) => break,
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });
    *state.scan_task.lock().await = Some(handle);
    Ok(())
}

/// 停止扫描。
#[tauri::command]
async fn bt_stop_scan(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(h) = state.scan_task.lock().await.take() {
        h.abort();
    }
    if let Some(c) = &state.central {
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
    let conn = bluelink_transport::connect_tcp(addr.trim())
        .await
        .map_err(|e| format!("连接失败：{e}"))?;
    let code_clean: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let peer = state
        .node
        .attach(conn, false, &code_clean)
        .await
        .map_err(|e| e.to_string())?;
    Ok(peer.name)
}

/// 连接扫描到的设备（访客模式）：code 为对方屏幕上显示的连接码，握手时校验。
#[tauri::command]
async fn bt_connect(
    state: State<'_, AppState>,
    device_id: String,
    code: String,
) -> Result<String, String> {
    let central = state
        .central
        .clone()
        .ok_or_else(|| "蓝牙不可用".to_string())?;
    let conn = central
        .connect(&device_id)
        .await
        .map_err(|e| format!("蓝牙连接失败：{e}"))?;
    let code_clean: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let peer = state
        .node
        .attach(conn, false, &code_clean)
        .await
        .map_err(|e| e.to_string())?;
    Ok(peer.name)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    state.node.disconnect().await;
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
    limit: Option<i64>,
) -> Result<Vec<Message>, String> {
    state
        .node
        .storage
        .list_messages(&conversation_id, None, limit.unwrap_or(200))
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
async fn ai_send(state: State<'_, AppState>, text: String) -> Result<Message, String> {
    let (tx, rx) = oneshot::channel::<()>();
    *state.ai_cancel.lock().await = Some(tx);
    let node = state.node.clone();
    tokio::select! {
        r = node.ai_send(&text) => r.map_err(|e| e.to_string()),
        _ = rx => Err("已停止生成".to_string()),
    }
}

#[tauri::command]
async fn ai_regenerate(state: State<'_, AppState>) -> Result<Message, String> {
    let (tx, rx) = oneshot::channel::<()>();
    *state.ai_cancel.lock().await = Some(tx);
    let node = state.node.clone();
    tokio::select! {
        r = node.ai_regenerate() => r.map_err(|e| e.to_string()),
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
    stream: bool,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    system_prompt: String,
}

#[tauri::command]
fn get_ai_config(state: State<'_, AppState>) -> Result<AiConfigView, String> {
    let cfg = load_ai_config(&state.node.storage);
    Ok(AiConfigView {
        has_key: !cfg.api_key.is_empty(),
        base_url: cfg.base_url,
        model: cfg.model,
        stream: cfg.stream,
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
        system_prompt: cfg.system_prompt,
    })
}

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

#[tauri::command]
async fn test_ai_connection(state: State<'_, AppState>) -> Result<String, String> {
    let cfg = load_ai_config(&state.node.storage);
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
}

#[tauri::command]
fn storage_stats(state: State<'_, AppState>) -> Result<StorageStatsView, String> {
    let s = state.node.storage_stats();
    Ok(StorageStatsView {
        chat_count: s.chat_count,
        chat_bytes: s.chat_bytes,
        files_bytes: s.files_bytes,
        files_count: s.files_count,
        cache_bytes: s.cache_bytes,
    })
}

#[tauri::command]
async fn clean_cache(state: State<'_, AppState>) -> Result<u64, String> {
    Ok(state.node.clean_cache().await)
}

#[tauri::command]
fn clear_chat_history(state: State<'_, AppState>) -> Result<(), String> {
    state.node.clear_chat_history().map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct FileInfoView {
    name: String,
    size: u64,
    path: Option<String>,
    exists: bool,
}

#[tauri::command]
fn file_info(state: State<'_, AppState>, message_id: String) -> Result<Option<FileInfoView>, String> {
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

#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    bluelink_services::open_file(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_file(path: String) -> Result<(), String> {
    bluelink_services::reveal_file(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

/// 复制文件到目标路径（文件菜单「另存为」，目标由保存对话框选定）。
#[tauri::command]
fn copy_file_as(src: String, dst: String) -> Result<(), String> {
    std::fs::copy(&src, &dst)
        .map(|_| ())
        .map_err(|e| format!("复制失败：{e}"))
}

/// 把粘贴的图片落盘暂存，返回路径（前端在点发送时按文件发送；剪贴板图片只在无文字时走这里）。
#[tauri::command]
fn stage_pasted_image(state: State<'_, AppState>, data_base64: String, ext: String) -> Result<String, String> {
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
    }
}

#[tauri::command]
async fn get_gateway_settings(state: State<'_, AppState>) -> Result<GatewaySettingsView, String> {
    let mut v = gateway_view(&state);
    v.local_running = state
        .local_api
        .lock()
        .await
        .as_ref()
        .map(|h| !h.is_finished())
        .unwrap_or(false);
    Ok(v)
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
    }
    if enabled {
        *slot = Some(local_api::spawn(state.node.clone()));
    }
    Ok(())
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
                }) => {
                    let _ = app.emit(
                        "connection_state",
                        serde_json::json!({"connected": connected, "peer_name": peer_name}),
                    );
                }
                Ok(Event::MessageNew(m)) => {
                    let _ = app.emit("message_new", m);
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
                Ok(Event::PeerTyping(t)) => {
                    let _ = app.emit("peer_typing", t);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
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
            // 蓝牙运行时：中心端（扫描/连出）+ 服务端（广播/被连入）。
            // 无适配器或未开启时为 None，应用照常启动，界面如实提示（bt_state）。
            let (central, server) = tauri::async_runtime::block_on(async {
                let central = bluelink_transport::ble::ble_central()
                    .await
                    .map(Arc::new)
                    .map_err(|e| {
                        eprintln!("蓝牙中心端不可用：{e}");
                        e
                    })
                    .ok();
                let server = match bluelink_transport::ble::start_ble_server(&name).await {
                    Ok((srv, mut inbound)) => {
                        let node_in = node.clone();
                        tauri::async_runtime::spawn(async move {
                            while let Some(conn) = inbound.recv().await {
                                // 宿主侧握手：校验对方给出的连接码是否为本机连接码
                                if let Err(e) = node_in.attach(conn, true, "").await {
                                    eprintln!("蓝牙入站连接握手失败：{e}");
                                }
                            }
                        });
                        Some(Arc::new(srv))
                    }
                    Err(e) => {
                        eprintln!("蓝牙服务端不可用：{e}");
                        None
                    }
                };
                (central, server)
            });
            app.manage(AppState {
                node,
                data_dir,
                ai_cancel: Mutex::new(None),
                central,
                server,
                scan_task: Mutex::new(None),
                listening: Mutex::new(None),
                local_api: Mutex::new(local_handle),
            });
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
            start_listening,
            connect_to,
            disconnect,
            send_text,
            send_file_path,
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
            ai_regenerate,
            ai_stop,
            get_ai_config,
            set_ai_config,
            test_ai_connection,
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
            clean_cache,
            clear_chat_history,
            file_info,
            open_file,
            reveal_file,
            copy_file_as,
            stage_pasted_image,
            get_gateway_settings,
            set_proxy_allow,
            set_local_api,
        ])
        .run(tauri::generate_context!())
        .expect("启动 BlueLink 失败");
}
