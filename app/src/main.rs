// BlueLink 桌面应用（Tauri 2）：命令层把前端调用转给服务层节点，
// 事件层把节点事件转发给前端。命令清单与详细设计第三章对应。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bluelink_ai::{AiClient, AiConfig, ChatMessage};
use bluelink_domain::{Conversation, MarkKind, Message, Tag};
use bluelink_services::{Event, Node};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, Mutex};

/// 应用状态：持有节点与正在进行的 AI 任务取消通道、监听任务。
struct AppState {
    node: Arc<Node>,
    ai_cancel: Mutex<Option<oneshot::Sender<()>>>,
    listening: Mutex<Option<tokio::task::JoinHandle<()>>>,
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

/// 开始监听（宿主模式）：在局域网 TCP 端口等待对方连接。
#[tauri::command]
async fn start_listening(state: State<'_, AppState>, port: Option<u16>) -> Result<String, String> {
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

/// 发起连接（访客模式）：addr 形如 192.168.1.10:7800，code 为对方连接码。
#[tauri::command]
async fn connect_to(
    state: State<'_, AppState>,
    addr: String,
    code: String,
) -> Result<String, String> {
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

/// AI 配置回显（不回传密钥明文，只报是否已配置）。
#[derive(Serialize)]
struct AiConfigView {
    base_url: String,
    model: String,
    has_key: bool,
}

#[tauri::command]
fn get_ai_config(state: State<'_, AppState>) -> Result<AiConfigView, String> {
    let s = &state.node.storage;
    Ok(AiConfigView {
        base_url: s
            .get_setting("ai_base")
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
        model: s
            .get_setting("ai_model")
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
        has_key: s
            .get_setting("ai_key")
            .map_err(|e| e.to_string())?
            .map(|k| !k.is_empty())
            .unwrap_or(false),
    })
}

#[tauri::command]
async fn set_ai_config(
    state: State<'_, AppState>,
    base_url: String,
    api_key: String,
    model: String,
) -> Result<(), String> {
    let s = &state.node.storage;
    s.set_setting("ai_base", &base_url)
        .map_err(|e| e.to_string())?;
    // 密钥留空表示不修改
    if !api_key.is_empty() {
        s.set_setting("ai_key", &api_key)
            .map_err(|e| e.to_string())?;
    }
    s.set_setting("ai_model", &model)
        .map_err(|e| e.to_string())?;
    let key = s
        .get_setting("ai_key")
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let cfg = AiConfig {
        base_url,
        api_key: key,
        model,
    };
    state
        .node
        .set_ai_config(if cfg.is_ready() { Some(cfg) } else { None })
        .await;
    Ok(())
}

#[tauri::command]
async fn test_ai_connection(state: State<'_, AppState>) -> Result<String, String> {
    let s = &state.node.storage;
    let cfg = AiConfig {
        base_url: s
            .get_setting("ai_base")
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
        api_key: s
            .get_setting("ai_key")
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
        model: s
            .get_setting("ai_model")
            .map_err(|e| e.to_string())?
            .unwrap_or_default(),
    };
    let client = AiClient::new(cfg);
    let reply = client
        .chat_once(&[ChatMessage {
            role: "user".into(),
            content: "ping".into(),
        }])
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "连接成功，接口回复：{}",
        reply.chars().take(50).collect::<String>()
    ))
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
            // 恢复已保存的 AI 配置
            let s = &node.storage;
            if let (Ok(Some(base)), Ok(Some(key)), Ok(Some(model))) = (
                s.get_setting("ai_base"),
                s.get_setting("ai_key"),
                s.get_setting("ai_model"),
            ) {
                let cfg = AiConfig {
                    base_url: base,
                    api_key: key,
                    model,
                };
                if cfg.is_ready() {
                    let n = node.clone();
                    tauri::async_runtime::block_on(async move { n.set_ai_config(Some(cfg)).await });
                }
            }
            forward_events(app.handle().clone(), node.clone());
            app.manage(AppState {
                node,
                ai_cancel: Mutex::new(None),
                listening: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_identity,
            start_listening,
            connect_to,
            disconnect,
            send_text,
            send_file_path,
            send_typing,
            list_conversations,
            list_messages,
            search_messages,
            set_message_tags,
            get_message_tags,
            list_tags,
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
        ])
        .run(tauri::generate_context!())
        .expect("启动 BlueLink 失败");
}
