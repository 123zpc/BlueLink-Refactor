//! 应用服务层：把协议、传输、存储、AI 编排成一个可用的节点（Node）。
//! 桌面端（Tauri）与演示 CLI 都只调用这一层，不直接碰底层 crate。

pub mod log;
pub use log::log_line;

use bluelink_ai::{AiClient, AiConfig, ChatMessage};
use bluelink_domain::*;
use bluelink_protocol::{ClipboardImage, Control, FileChunk, Frame, CHUNK_SIZE};
use bluelink_storage::Storage;
use bluelink_transport::Conn;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, oneshot, Mutex};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("存储错误：{0}")]
    Storage(#[from] bluelink_storage::StorageError),
    #[error("传输错误：{0}")]
    Transport(#[from] bluelink_transport::TransportError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("尚未连接对方设备")]
    NotConnected,
    #[error("连接码不正确")]
    BadCode,
    #[error("对方拒绝了连接")]
    Rejected,
    #[error("{0}")]
    Ai(#[from] bluelink_ai::AiError),
    #[error("{0}")]
    Other(String),
}

/// 服务层事件（Tauri 层订阅后转发给前端，事件名与详细设计第三章对应）。
#[derive(Debug, Clone)]
pub enum Event {
    ConnectionChanged {
        connected: bool,
        peer_name: Option<String>,
    },
    MessageNew(Message),
    MessageStatus {
        id: String,
        status: MessageStatus,
    },
    TransferProgress {
        transfer_id: String,
        msg_id: String,
        received: u64,
        total: u64,
    },
    TransferState {
        transfer_id: String,
        state: String,
    },
    AiChunk {
        conversation_id: String,
        delta: String,
    },
    AiDone {
        conversation_id: String,
        message: Message,
    },
    AiError {
        conversation_id: String,
        error: String,
    },
    PeerTyping(bool),
    /// 剪贴板同步状态变化（本机开关/对方开关/连接状态任一变化时发，前端据此刷新状态文案）。
    ClipboardState {
        local: bool,
        peer: bool,
        connected: bool,
    },
    /// 刚把对方的一份剪贴板内容写入本机（kind 为 "text" 或 "image"）。
    ClipboardSynced {
        kind: String,
    },
}

/// AI 来源：Auto = 本机优先、否则经对端转发；Local = 只用本机配置；Peer = 只用连接设备的 AI。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSource {
    Auto,
    Local,
    Peer,
}

impl AiSource {
    /// 解析前端传来的来源字符串（default/auto 归 Auto）。
    pub fn parse(s: &str) -> AiSource {
        match s {
            "local" => AiSource::Local,
            "peer" => AiSource::Peer,
            _ => AiSource::Auto,
        }
    }
}

/// 设置页存储区数据（聊天记录/接收文件/缓存占用）。
#[derive(Debug, Clone, Copy)]
pub struct StorageStats {
    pub chat_count: i64,
    pub chat_bytes: i64,
    pub files_bytes: u64,
    pub files_count: u64,
    pub cache_bytes: u64,
}

/// 一条文件消息对应的本地文件信息。
#[derive(Debug, Clone)]
pub struct MessageFileInfo {
    pub name: String,
    pub size: u64,
    pub path: Option<String>,
    pub exists: bool,
}

/// 用系统默认程序打开文件（文件菜单「打开」）。
pub fn open_file(path: &Path) -> Result<(), ServiceError> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.spawn()?;
    Ok(())
}

/// 在系统文件管理器中定位文件（文件菜单「打开文件位置」）。
pub fn reveal_file(path: &Path) -> Result<(), ServiceError> {
    // 各平台定位文件的方式不同，统一收成一个结果返回（不用 return，避免平台分支触发 needless_return）
    #[cfg(target_os = "windows")]
    let result = {
        // explorer 对 /select 的成功返回码也不稳定，只负责拉起进程
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()?;
        Ok(())
    };
    #[cfg(target_os = "macos")]
    let result = {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()?;
        Ok(())
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = {
        let dir = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
        Ok(())
    };
    result
}

/// 已知压缩格式（这些扩展名的分片直传，不再压缩）。
const KNOWN_COMPRESSED: &[&str] = &[
    "zip", "7z", "rar", "gz", "bz2", "xz", "jpg", "jpeg", "png", "gif", "webp", "mp4", "mkv",
    "mp3", "aac", "pdf", "docx", "xlsx", "pptx",
];

/// 计算文件的 SHA-256（流式读取，不把整文件读入内存）。
pub fn sha256_file(path: &Path) -> Result<String, ServiceError> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// 由文件哈希与对端设备派生稳定的传输 ID：同一文件重发时复用，实现断点续传。
pub fn derive_transfer_id(file_sha: &str, peer_id: &str) -> String {
    let d = Sha256::digest(format!("{file_sha}:{peer_id}").as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
    )
}

/// 尝试压缩分片：已知压缩格式或压不动时返回 None（直传）。
fn try_compress(data: &[u8], file_name: &str) -> Option<Vec<u8>> {
    let ext = Path::new(file_name).extension()?.to_str()?.to_lowercase();
    if KNOWN_COMPRESSED.contains(&ext.as_str()) {
        return None;
    }
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).ok()?;
    let out = enc.finish().ok()?;
    if out.len() < data.len() {
        Some(out)
    } else {
        None
    }
}

fn decompress(data: &[u8]) -> Result<Vec<u8>, ServiceError> {
    use flate2::read::ZlibDecoder;
    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

/// 无条件 zlib 压缩（剪贴板图片用；压不动时调用方自行回落直传）。
fn zlib_compress(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data).ok()?;
    enc.finish().ok()
}

/// 剪贴板单份内容上限（2MB）：文字按字节数、图片按压缩后实际发送大小算，
/// 超限跳过同步并记日志（不打扰用户）。
pub const CLIPBOARD_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 一份剪贴板内容（本轮只同步文字与位图，文件列表不做）。
#[derive(Debug, Clone, PartialEq)]
enum ClipContent {
    Text(String),
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

impl ClipContent {
    fn kind(&self) -> &'static str {
        match self {
            ClipContent::Text(_) => "text",
            ClipContent::Image { .. } => "image",
        }
    }

    /// 内容哈希（防回环与去重都用它）：文字哈希原文，图片哈希宽高 + RGBA。
    fn hash(&self) -> String {
        let mut h = Sha256::new();
        match self {
            ClipContent::Text(t) => {
                h.update(b"text:");
                h.update(t.as_bytes());
            }
            ClipContent::Image {
                width,
                height,
                rgba,
            } => {
                h.update(b"image:");
                h.update(width.to_le_bytes());
                h.update(height.to_le_bytes());
                h.update(rgba);
            }
        }
        format!("{:x}", h.finalize())
    }
}

/// 图片内容的原始 SHA-256 摘要（帧里携带的 hash 字段，与 ClipContent::hash 同算法）。
fn clip_image_digest(width: u32, height: u32, rgba: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"image:");
    h.update(width.to_le_bytes());
    h.update(height.to_le_bytes());
    h.update(rgba);
    h.finalize().into()
}

/// 读系统剪贴板：与粘贴规则同口径——先文字，纯图片才取图片；空剪贴板返回 None。
/// 句柄由 Node 长驻持有（见 clip_handle 字段注释），不要在这里新建临时句柄。
fn clip_read_with(cb: &mut arboard::Clipboard) -> Option<ClipContent> {
    if let Ok(t) = cb.get_text() {
        if !t.is_empty() {
            return Some(ClipContent::Text(t));
        }
    }
    if let Ok(img) = cb.get_image() {
        return Some(ClipContent::Image {
            width: img.width as u32,
            height: img.height as u32,
            rgba: img.bytes.into_owned(),
        });
    }
    None
}

/// 写系统剪贴板（句柄由 Node 长驻持有，理由同上）。
fn clip_write_with(cb: &mut arboard::Clipboard, content: &ClipContent) -> Result<(), String> {
    match content {
        ClipContent::Text(t) => cb.set_text(t.clone()).map_err(|e| e.to_string()),
        ClipContent::Image {
            width,
            height,
            rgba,
        } => cb
            .set_image(arboard::ImageData {
                width: *width as usize,
                height: *height as usize,
                bytes: std::borrow::Cow::Borrowed(rgba.as_slice()),
            })
            .map_err(|e| e.to_string()),
    }
}

/// 剪贴板同步的防回环状态：last_seen 是最近见过的本机剪贴板内容哈希
/// （自己复制后发出的、或从对方写入的都算），suppress 是最近一次从对方
/// 写入的内容哈希——监视到它时绝不回发，避免两台机器来回震荡。
#[derive(Default)]
struct ClipGuard {
    last_seen: Option<String>,
    suppress: Option<String>,
}

/// 接收中的文件状态。
struct RecvState {
    name: String,
    total: u64,
    sha256: String,
    msg_id: String,
    file_id: String,
    part_path: PathBuf,
    ranges: Ranges,
    written: u64,
}

/// 一个对端会话。
struct PeerSession {
    conn: Arc<Conn>,
    peer: Device,
    conversation_id: String,
}

/// 节点：本机身份 + 存储 + 当前连接 + AI 配置。
pub struct Node {
    pub device_id: String,
    pub device_name: String,
    pub storage: Arc<Storage>,
    /// 接收文件保存目录：可在设置里修改（运行时可换，存在 RwLock 里）。
    download_dir: std::sync::RwLock<PathBuf>,
    /// 默认保存目录（数据目录下的 downloads），「恢复默认」时回到这里。
    default_download_dir: PathBuf,
    peer: Mutex<Option<Arc<PeerSession>>>,
    events: broadcast::Sender<Event>,
    ai_cfg: Mutex<Option<AiConfig>>,
    accept_waiters: Mutex<HashMap<String, oneshot::Sender<Ranges>>>,
    verify_waiters: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    recv_files: Mutex<HashMap<String, RecvState>>,
    proxy_waiters: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Result<String, String>>>>,
    /// 被用户取消的发送中传输 ID（发送循环逐片检查）。
    cancelled: Mutex<HashSet<String>>,
    /// 对方剪贴板同步开关状态（连接建立后由对方通告，断开时复位）。
    clip_peer_on: AtomicBool,
    /// 剪贴板同步防回环状态（监视任务与接收处理共用）。
    clip_guard: Mutex<ClipGuard>,
    /// 全进程长驻的系统剪贴板句柄：arboard 在 X11 上由后台线程持有选择内容，
    /// 最后一个句柄被释放后内容立即失效——所以句柄必须常驻、绝不随单次读写销毁。
    /// 用 std Mutex：只在阻塞线程里短暂持有，不跨 await。
    clip_handle: std::sync::Mutex<Option<arboard::Clipboard>>,
}

impl Node {
    /// 创建节点。identity_path 中持久化设备 ID，保证重启后连接码不变。
    pub fn new(data_dir: &Path, device_name: &str) -> Result<Arc<Self>, ServiceError> {
        std::fs::create_dir_all(data_dir)?;
        log::init_log(data_dir);
        log::log_line(
            "app",
            &format!(
                "节点启动：设备名「{device_name}」，数据目录 {}",
                data_dir.display()
            ),
        );
        let storage = Arc::new(Storage::open(
            data_dir
                .join("bluelink.db")
                .to_str()
                .unwrap_or("bluelink.db"),
        )?);
        // 保存目录：优先用设置里用户自选的目录（建不出来时回落默认），否则默认目录
        let default_download_dir = data_dir.join("downloads");
        let saved_dir = storage
            .get_setting("download_dir")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);
        let download_dir = match saved_dir {
            Some(dir) if std::fs::create_dir_all(&dir).is_ok() => dir,
            _ => {
                std::fs::create_dir_all(&default_download_dir)?;
                default_download_dir.clone()
            }
        };
        let id_path = data_dir.join("device_id.txt");
        let device_id = if id_path.exists() {
            std::fs::read_to_string(&id_path)?.trim().to_string()
        } else {
            let id = new_id();
            std::fs::write(&id_path, &id)?;
            id
        };
        let (events, _) = broadcast::channel(512);
        Ok(Arc::new(Self {
            device_id,
            device_name: device_name.to_string(),
            storage,
            download_dir: std::sync::RwLock::new(download_dir),
            default_download_dir,
            peer: Mutex::new(None),
            events,
            ai_cfg: Mutex::new(None),
            accept_waiters: Mutex::new(HashMap::new()),
            verify_waiters: Mutex::new(HashMap::new()),
            recv_files: Mutex::new(HashMap::new()),
            proxy_waiters: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(HashSet::new()),
            clip_peer_on: AtomicBool::new(false),
            clip_guard: Mutex::new(ClipGuard::default()),
            clip_handle: std::sync::Mutex::new(None),
        }))
    }

    /// 本机连接码。
    pub fn my_code(&self) -> String {
        connection_code(&self.device_id)
    }

    /// 当前接收文件保存目录（接收文件落地处）。
    pub fn download_dir(&self) -> PathBuf {
        self.download_dir
            .read()
            .map(|d| d.clone())
            .unwrap_or_else(|_| self.default_download_dir.clone())
    }

    /// 修改保存目录：传 None 恢复默认。目录会被创建并持久化到设置，立即生效。
    pub fn set_download_dir(&self, dir: Option<&Path>) -> Result<PathBuf, ServiceError> {
        let target = match dir {
            Some(p) => p.to_path_buf(),
            None => self.default_download_dir.clone(),
        };
        std::fs::create_dir_all(&target)?;
        if let Ok(mut cur) = self.download_dir.write() {
            *cur = target.clone();
        }
        let saved = if dir.is_some() {
            target.to_string_lossy().to_string()
        } else {
            String::new()
        };
        self.storage.set_setting("download_dir", &saved)?;
        log::log_line(
            "files",
            &format!("接收文件保存目录改为 {}", target.display()),
        );
        Ok(target)
    }

    /// 订阅事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    /// 设置 AI 配置（None 表示未配置）。
    pub async fn set_ai_config(&self, cfg: Option<AiConfig>) {
        *self.ai_cfg.lock().await = cfg;
    }

    async fn session(&self) -> Result<Arc<PeerSession>, ServiceError> {
        self.peer
            .lock()
            .await
            .clone()
            .ok_or(ServiceError::NotConnected)
    }

    /// 是否已连接。
    pub async fn is_connected(&self) -> bool {
        self.peer.lock().await.is_some()
    }

    /// 握手本体（attach 的限时区）：宿主侧等 Hello 并校验连接码，
    /// 发起侧发 Hello 等 Ack。块内的 return 即握手失败出口。
    async fn handshake(
        &self,
        conn: &Conn,
        host: bool,
        code_from_peer_or_target: &str,
    ) -> Result<Device, ServiceError> {
        let peer = if host {
            // 宿主侧：等对方 Hello，校验连接码
            log::log_line("conn", "握手步骤5/6：等待对方 Hello");
            match conn.recv().await? {
                Frame::Control(Control::Hello {
                    device_id,
                    device_name,
                    code,
                    ..
                }) => {
                    log::log_line(
                        "conn",
                        &format!("握手步骤5/6：收到 Hello（对方 {device_name}），校验连接码"),
                    );
                    if code != self.my_code() {
                        let _ = conn
                            .send(&Frame::Control(Control::HelloAck {
                                device_id: self.device_id.clone(),
                                device_name: self.device_name.clone(),
                                accepted: false,
                                reason: Some("连接码不正确".into()),
                            }))
                            .await;
                        return Err(ServiceError::BadCode);
                    }
                    conn.send(&Frame::Control(Control::HelloAck {
                        device_id: self.device_id.clone(),
                        device_name: self.device_name.clone(),
                        accepted: true,
                        reason: None,
                    }))
                    .await?;
                    log::log_line(
                        "conn",
                        &format!("握手步骤6/6：连接码校验通过，已回 HelloAck，握手完成（对方 {device_name}）"),
                    );
                    Device {
                        device_id,
                        name: device_name,
                        code: String::new(),
                        last_seen: now_ms(),
                    }
                }
                _ => return Err(ServiceError::Other("握手失败：未收到 Hello".into())),
            }
        } else {
            // 发起侧：发 Hello（带目标连接码），等 Ack
            log::log_line(
                "conn",
                &format!("握手步骤5/6：发出 Hello（本机名 {}）", self.device_name),
            );
            conn.send(&Frame::Control(Control::Hello {
                device_id: self.device_id.clone(),
                device_name: self.device_name.clone(),
                protocol: 2,
                code: code_from_peer_or_target.to_string(),
            }))
            .await?;
            match conn.recv().await? {
                Frame::Control(Control::HelloAck {
                    device_id,
                    device_name,
                    accepted,
                    ..
                }) => {
                    if !accepted {
                        log::log_line("conn", "握手失败于步骤6/6：对方拒绝（连接码不正确）");
                        return Err(ServiceError::Rejected);
                    }
                    log::log_line(
                        "conn",
                        &format!("握手步骤6/6：收到 HelloAck，握手完成（对方 {device_name}）"),
                    );
                    Device {
                        device_id,
                        name: device_name,
                        code: String::new(),
                        last_seen: now_ms(),
                    }
                }
                _ => return Err(ServiceError::Other("握手失败：未收到 HelloAck".into())),
            }
        };
        Ok(peer)
    }

    /// 在已建立的字节流上完成握手并接管连接（宿主侧 host=true 时校验对方给出的连接码）。
    pub async fn attach(
        self: &Arc<Self>,
        conn: Conn,
        host: bool,
        code_from_peer_or_target: &str,
    ) -> Result<Device, ServiceError> {
        let conn = Arc::new(conn);
        // 握手限时：连上不应答的端点（接受连接但不说协议）时 recv 会无限等，
        // 20 秒未完成握手即判失败（连接侧的取消也依赖尝试能自己收尾）。
        let peer = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.handshake(&conn, host, code_from_peer_or_target),
        )
        .await
        .map_err(|_| ServiceError::Other("握手超时：对方未在 20 秒内响应".into()))??;

        // 落库设备与会话（会话 ID 以对端设备 ID 派生，稳定唯一）
        let mut peer_dev = peer.clone();
        peer_dev.code = connection_code(&peer.device_id);
        self.storage.upsert_device(&peer_dev)?;
        let conversation_id = format!("peer-{}", peer.device_id);
        self.storage.create_conversation(&Conversation {
            id: conversation_id.clone(),
            kind: ConversationKind::Peer,
            peer_device_id: Some(peer.device_id.clone()),
            title: peer.name.clone(),
            draft: String::new(),
            last_read_id: None,
            created_at: now_ms(),
        })?;

        let session = Arc::new(PeerSession {
            conn: conn.clone(),
            peer,
            conversation_id,
        });
        *self.peer.lock().await = Some(session.clone());
        self.emit(Event::ConnectionChanged {
            connected: true,
            peer_name: Some(session.peer.name.clone()),
        });

        // 接收循环
        let peer_out = session.peer.clone();
        let node = self.clone();
        let recv_session = session.clone();
        tokio::spawn(async move { node.recv_loop(recv_session).await });

        // 剪贴板同步：连接建立后先把本机开关状态通告对方（对方也会发来它的），
        // 再启动本机剪贴板监视任务（任务里再按「双方都开」决定是否真正读写）
        let _ = session
            .conn
            .send(&Frame::Control(Control::ClipboardState {
                enabled: self.clipboard_local_on(),
            }))
            .await;
        self.emit_clipboard_state(true);
        self.spawn_clipboard_poll(session);
        Ok(peer_out)
    }

    /// 断开当前连接：先关底层连接（对端读到 EOF 随之结束会话），再清本机会话。
    /// 只清指针不关连接的话，对端会一直以为还连着（2026-11-19 取消迟到成功实测撞出）。
    pub async fn disconnect(&self) {
        let session = self.peer.lock().await.take();
        if let Some(s) = &session {
            s.conn.close().await;
        }
        drop(session);
        self.clip_peer_on.store(false, Ordering::Relaxed);
        self.emit(Event::ConnectionChanged {
            connected: false,
            peer_name: None,
        });
        self.emit_clipboard_state(false);
    }

    async fn recv_loop(self: Arc<Self>, session: Arc<PeerSession>) {
        while let Ok(frame) = session.conn.recv().await {
            if let Err(e) = self.handle_frame(&session, frame).await {
                eprintln!("处理帧出错：{e}");
            }
        }
        // 连接断开：清理会话并通知
        let mut guard = self.peer.lock().await;
        if let Some(cur) = &*guard {
            if Arc::ptr_eq(cur, &session) {
                *guard = None;
                self.clip_peer_on.store(false, Ordering::Relaxed);
                self.emit(Event::ConnectionChanged {
                    connected: false,
                    peer_name: None,
                });
                self.emit_clipboard_state(false);
            }
        }
    }

    async fn handle_frame(
        self: &Arc<Self>,
        session: &Arc<PeerSession>,
        frame: Frame,
    ) -> Result<(), ServiceError> {
        match frame {
            Frame::Control(Control::Ping) => {
                session.conn.send(&Frame::Control(Control::Pong)).await?;
            }
            Frame::Control(Control::Pong)
            | Frame::Control(Control::Hello { .. })
            | Frame::Control(Control::HelloAck { .. }) => {}
            Frame::Control(Control::Typing { typing }) => {
                self.emit(Event::PeerTyping(typing));
            }
            Frame::Control(Control::ClipboardState { enabled }) => {
                let prev = self.clip_peer_on.swap(enabled, Ordering::Relaxed);
                if prev != enabled {
                    log::log_line(
                        "clipboard",
                        &format!(
                            "对方剪贴板同步开关：{}",
                            if enabled { "已开启" } else { "未开启" }
                        ),
                    );
                }
                self.emit_clipboard_state(true);
            }
            Frame::Control(Control::ClipboardText { text, .. }) => {
                self.apply_remote_clipboard(ClipContent::Text(text)).await;
            }
            Frame::ClipboardImage(img) => {
                self.apply_remote_clipboard_image(img).await;
            }
            Frame::Control(Control::ChatText {
                msg_id,
                text,
                reply_to,
                ts,
            }) => {
                let m = Message {
                    id: msg_id.clone(),
                    conversation_id: session.conversation_id.clone(),
                    sender: Sender::Peer,
                    kind: MessageKind::Text,
                    body: text,
                    file_id: None,
                    reply_to,
                    status: MessageStatus::Received,
                    created_at: ts,
                };
                self.storage.insert_message(&m)?;
                self.emit(Event::MessageNew(m));
                session
                    .conn
                    .send(&Frame::Control(Control::Delivered { msg_id }))
                    .await?;
            }
            Frame::Control(Control::Delivered { msg_id }) => {
                self.storage
                    .update_message_status(&msg_id, MessageStatus::Delivered)?;
                self.emit(Event::MessageStatus {
                    id: msg_id,
                    status: MessageStatus::Delivered,
                });
            }
            Frame::Control(Control::FileOffer {
                transfer_id,
                msg_id,
                name,
                size,
                sha256,
                mime,
            }) => {
                self.on_file_offer(session, transfer_id, msg_id, name, size, sha256, mime)
                    .await?;
            }
            Frame::Control(Control::FileAccept {
                transfer_id,
                received,
            }) => {
                if let Some(tx) = self.accept_waiters.lock().await.remove(&transfer_id) {
                    let _ = tx.send(received);
                }
            }
            Frame::Chunk(chunk) => {
                self.on_file_chunk(session, chunk).await?;
            }
            Frame::Control(Control::FileDone { transfer_id }) => {
                self.on_file_done(session, transfer_id).await?;
            }
            Frame::Control(Control::FileVerify { transfer_id, ok }) => {
                if let Some(tx) = self.verify_waiters.lock().await.remove(&transfer_id) {
                    let _ = tx.send(ok);
                }
            }
            Frame::Control(Control::FileCancel { transfer_id }) => {
                // 若本机正在发送这条传输：置取消标记，发送循环下一片即停
                self.cancelled.lock().await.insert(transfer_id.clone());
                // 对方取消了发送：清掉接收中的半成品（.part 与内存状态）
                if let Some(st) = self.recv_files.lock().await.remove(&transfer_id) {
                    let _ = std::fs::remove_file(&st.part_path);
                    let _ = self
                        .storage
                        .update_message_status(&st.msg_id, MessageStatus::Failed);
                    self.emit(Event::MessageStatus {
                        id: st.msg_id,
                        status: MessageStatus::Failed,
                    });
                }
                self.emit(Event::TransferState {
                    transfer_id,
                    state: "cancelled".into(),
                });
            }
            Frame::Control(Control::AiProxyRequest {
                request_id,
                messages,
                ..
            }) => {
                let node = self.clone();
                let session2 = session.clone();
                tokio::spawn(async move {
                    let _ = node.serve_ai_proxy(session2, request_id, messages).await;
                });
            }
            Frame::Control(Control::AiProxyChunk { request_id, delta }) => {
                self.handle_ai_proxy_frame(Control::AiProxyChunk { request_id, delta })
                    .await;
            }
            Frame::Control(Control::AiProxyDone { request_id, error }) => {
                self.handle_ai_proxy_frame(Control::AiProxyDone { request_id, error })
                    .await;
            }
        }
        Ok(())
    }

    // ---------- 聊天 ----------
    /// 发送文本消息，返回落库后的消息。
    pub async fn send_text(
        &self,
        text: &str,
        reply_to: Option<String>,
    ) -> Result<Message, ServiceError> {
        let session = self.session().await?;
        let m = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: reply_to.clone(),
            status: MessageStatus::Sending,
            created_at: now_ms(),
        };
        self.storage.insert_message(&m)?;
        if let Err(e) = session
            .conn
            .send(&Frame::Control(Control::ChatText {
                msg_id: m.id.clone(),
                text: m.body.clone(),
                reply_to,
                ts: m.created_at,
            }))
            .await
        {
            // 发送失败要如实落状态，前端据此显示「发送失败 · 点击重试」
            let _ = self
                .storage
                .update_message_status(&m.id, MessageStatus::Failed);
            self.emit(Event::MessageStatus {
                id: m.id.clone(),
                status: MessageStatus::Failed,
            });
            return Err(e.into());
        }
        self.storage
            .update_message_status(&m.id, MessageStatus::Sent)?;
        self.emit(Event::MessageStatus {
            id: m.id.clone(),
            status: MessageStatus::Sent,
        });
        Ok(self.storage.get_message(&m.id)?.unwrap_or(m))
    }

    /// 取走传输的取消标记（发送循环逐片调用）。
    async fn take_cancelled(&self, transfer_id: &str) -> bool {
        self.cancelled.lock().await.remove(transfer_id)
    }

    /// 取消收尾：消息标失败、通知前端传输已取消。
    async fn finish_cancelled(&self, transfer_id: &str, msg_id: &str) {
        let _ = self
            .storage
            .update_message_status(msg_id, MessageStatus::Failed);
        self.emit(Event::MessageStatus {
            id: msg_id.to_string(),
            status: MessageStatus::Failed,
        });
        self.emit(Event::TransferState {
            transfer_id: transfer_id.to_string(),
            state: "cancelled".into(),
        });
    }

    /// 取消一条进行中的传输（本机发送或接收都可发起）：置标记并通知对方。
    pub async fn cancel_transfer(&self, transfer_id: &str) {
        self.cancelled.lock().await.insert(transfer_id.to_string());
        if let Ok(s) = self.session().await {
            let _ = s
                .conn
                .send(&Frame::Control(Control::FileCancel {
                    transfer_id: transfer_id.to_string(),
                }))
                .await;
        }
    }

    /// 重发一条失败的消息：文本按原 ID 重新走发送；文件按原路径重新发起传输
    ///（会产生一条新的文件消息，前端负责移除旧的失败气泡）。
    pub async fn resend_message(self: &Arc<Self>, message_id: &str) -> Result<(), ServiceError> {
        let m = self
            .storage
            .get_message(message_id)?
            .ok_or_else(|| ServiceError::Other("消息不存在".into()))?;
        match m.kind {
            MessageKind::Text => {
                let session = self.session().await?;
                self.storage
                    .update_message_status(&m.id, MessageStatus::Sending)?;
                self.emit(Event::MessageStatus {
                    id: m.id.clone(),
                    status: MessageStatus::Sending,
                });
                match session
                    .conn
                    .send(&Frame::Control(Control::ChatText {
                        msg_id: m.id.clone(),
                        text: m.body.clone(),
                        reply_to: m.reply_to.clone(),
                        ts: m.created_at,
                    }))
                    .await
                {
                    Ok(()) => {
                        self.storage
                            .update_message_status(&m.id, MessageStatus::Sent)?;
                        self.emit(Event::MessageStatus {
                            id: m.id.clone(),
                            status: MessageStatus::Sent,
                        });
                        Ok(())
                    }
                    Err(e) => {
                        let _ = self
                            .storage
                            .update_message_status(&m.id, MessageStatus::Failed);
                        self.emit(Event::MessageStatus {
                            id: m.id.clone(),
                            status: MessageStatus::Failed,
                        });
                        Err(e.into())
                    }
                }
            }
            MessageKind::File => {
                let file_id = m
                    .file_id
                    .clone()
                    .ok_or_else(|| ServiceError::Other("文件记录缺失".into()))?;
                let meta = self
                    .storage
                    .get_file(&file_id)?
                    .ok_or_else(|| ServiceError::Other("文件记录缺失".into()))?;
                if meta.path.is_empty() || !Path::new(&meta.path).exists() {
                    return Err(ServiceError::Other("原文件已不存在，无法重发".into()));
                }
                let node = self.clone();
                let path = PathBuf::from(&meta.path);
                tokio::spawn(async move {
                    let _ = node.send_file(&path).await;
                });
                Ok(())
            }
            MessageKind::System => Err(ServiceError::Other("系统消息不能重发".into())),
        }
    }

    /// 发送正在输入状态。
    pub async fn send_typing(&self, typing: bool) {
        if let Ok(s) = self.session().await {
            let _ = s
                .conn
                .send(&Frame::Control(Control::Typing { typing }))
                .await;
        }
    }

    // ---------- 剪贴板同步 ----------
    /// 本机剪贴板同步开关（设置键 clipboard_sync，默认关）。
    pub fn clipboard_local_on(&self) -> bool {
        self.storage
            .get_setting("clipboard_sync")
            .map(|v| v.as_deref() == Some("1"))
            .unwrap_or(false)
    }

    /// 对方剪贴板同步开关（对方经 ClipboardState 通告，断开时复位为关）。
    pub fn clipboard_peer_on(&self) -> bool {
        self.clip_peer_on.load(Ordering::Relaxed)
    }

    /// 剪贴板同步三态快照（本机开关、对方开关、是否已连接），供界面展示状态。
    pub async fn clipboard_status(&self) -> (bool, bool, bool) {
        (
            self.clipboard_local_on(),
            self.clipboard_peer_on(),
            self.is_connected().await,
        )
    }

    /// 取长驻剪贴板句柄执行闭包（句柄不存在时懒创建；系统剪贴板不可用返回 None）。
    /// 只在阻塞线程里调用：std Mutex 不跨 await。
    fn with_clipboard<R>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
        let mut guard = self.clip_handle.lock().ok()?;
        if guard.is_none() {
            *guard = arboard::Clipboard::new().ok();
        }
        guard.as_mut().map(f)
    }

    fn emit_clipboard_state(&self, connected: bool) {
        self.emit(Event::ClipboardState {
            local: self.clipboard_local_on(),
            peer: self.clipboard_peer_on(),
            connected,
        });
    }

    /// 切换剪贴板同步开关：落库、即时生效（不用重启）；已连接时立刻通告对方。
    pub async fn set_clipboard_sync(&self, on: bool) -> Result<(), ServiceError> {
        self.storage
            .set_setting("clipboard_sync", if on { "1" } else { "0" })?;
        log::log_line(
            "clipboard",
            &format!("本机剪贴板同步开关：{}", if on { "开启" } else { "关闭" }),
        );
        if let Ok(s) = self.session().await {
            let _ = s
                .conn
                .send(&Frame::Control(Control::ClipboardState { enabled: on }))
                .await;
            self.emit_clipboard_state(true);
        }
        Ok(())
    }

    /// 本机剪贴板监视任务（随会话启动、会话结束自动退出）：每 700ms 轮询一次。
    /// 隐私底线：本机开关关闭时连读都不读；对方未开启时读了也不发，同样不读。
    fn spawn_clipboard_poll(self: &Arc<Self>, session: Arc<PeerSession>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(700));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                // 会话已断开或已被新会话替换：退出监视
                let alive = {
                    let guard = node.peer.lock().await;
                    matches!(&*guard, Some(cur) if Arc::ptr_eq(cur, &session))
                };
                if !alive {
                    break;
                }
                if !node.clipboard_local_on() || !node.clipboard_peer_on() {
                    continue;
                }
                node.clipboard_poll_once(&session).await;
            }
        });
    }

    /// 一次监视：读本机剪贴板，内容变了且不是刚从对方写入的才发送。
    async fn clipboard_poll_once(self: &Arc<Self>, session: &Arc<PeerSession>) {
        // 剪贴板读取是同步系统调用，放阻塞线程执行，避免卡住异步运行时
        let node = self.clone();
        let content =
            tokio::task::spawn_blocking(move || node.with_clipboard(clip_read_with).flatten())
                .await
                .ok()
                .flatten();
        let Some(content) = content else { return };
        let hash = content.hash();
        {
            let mut g = self.clip_guard.lock().await;
            if g.last_seen.as_deref() == Some(hash.as_str()) {
                return; // 没变化
            }
            g.last_seen = Some(hash.clone());
            if g.suppress.as_deref() == Some(hash.as_str()) {
                // 这是刚从对方同步写入本机的内容，绝不回发（防回环）
                return;
            }
        }
        match content {
            ClipContent::Text(text) => {
                if text.len() > CLIPBOARD_MAX_BYTES {
                    log::log_line(
                        "clipboard",
                        &format!("剪贴板文字 {} 字节，超过 2MB 上限，跳过同步", text.len()),
                    );
                    return;
                }
                let n = text.len();
                if session
                    .conn
                    .send(&Frame::Control(Control::ClipboardText { hash, text }))
                    .await
                    .is_ok()
                {
                    log::log_line("clipboard", &format!("剪贴板同步：已发送文字（{n} 字节）"));
                }
            }
            ClipContent::Image {
                width,
                height,
                rgba,
            } => {
                // 位图先 zlib 压缩（截图类内容压缩率高），上限按实际发送大小算
                let digest = clip_image_digest(width, height, &rgba);
                let compressed = zlib_compress(&rgba).filter(|c| c.len() < rgba.len());
                let (data, was_compressed) = match &compressed {
                    Some(c) => (c.clone(), true),
                    None => (rgba, false),
                };
                if data.len() > CLIPBOARD_MAX_BYTES {
                    log::log_line(
                        "clipboard",
                        &format!(
                            "剪贴板图片 {width}x{height}（发送大小 {} 字节），超过 2MB 上限，跳过同步",
                            data.len()
                        ),
                    );
                    return;
                }
                let n = data.len();
                let frame = Frame::ClipboardImage(ClipboardImage {
                    hash: digest,
                    width,
                    height,
                    compressed: was_compressed,
                    data,
                });
                if session.conn.send(&frame).await.is_ok() {
                    log::log_line(
                        "clipboard",
                        &format!("剪贴板同步：已发送图片（{width}x{height}，{n} 字节）"),
                    );
                }
            }
        }
    }

    /// 收到对方剪贴板图片帧：校验尺寸、解压成 RGBA 后走统一应用路径。
    async fn apply_remote_clipboard_image(self: &Arc<Self>, img: ClipboardImage) {
        let ClipboardImage {
            width,
            height,
            compressed,
            data,
            ..
        } = img;
        if data.len() > CLIPBOARD_MAX_BYTES {
            log::log_line(
                "clipboard",
                &format!(
                    "收到对方剪贴板图片（{} 字节），超过 2MB 上限，已忽略",
                    data.len()
                ),
            );
            return;
        }
        // RGBA 长度必须与宽高严格对应，且给解压结果设 64MB 安全上限防异常帧
        let expect = (width as u64)
            .checked_mul(height as u64)
            .and_then(|n| n.checked_mul(4));
        let rgba = if compressed {
            match tokio::task::spawn_blocking(move || {
                use flate2::read::ZlibDecoder;
                let mut dec = ZlibDecoder::new(&data[..]);
                let mut out = Vec::new();
                let mut limited = (&mut dec).take(64 * 1024 * 1024 + 1);
                limited.read_to_end(&mut out).map(|_| out)
            })
            .await
            {
                Ok(Ok(v)) => v,
                _ => {
                    log::log_line("clipboard", "收到对方剪贴板图片，解压失败，已忽略");
                    return;
                }
            }
        } else {
            data
        };
        if expect != Some(rgba.len() as u64) || rgba.len() > 64 * 1024 * 1024 {
            log::log_line(
                "clipboard",
                &format!(
                    "收到对方剪贴板图片，尺寸不符（{width}x{height} 期望 {expect:?} 字节，实际 {}），已忽略",
                    rgba.len()
                ),
            );
            return;
        }
        self.apply_remote_clipboard(ClipContent::Image {
            width,
            height,
            rgba,
        })
        .await;
    }

    /// 把对方的一份剪贴板内容写入本机剪贴板（本机没开同步则绝不写），
    /// 并登记其哈希：本机监视到这份内容时不会再把它发回去。
    async fn apply_remote_clipboard(self: &Arc<Self>, content: ClipContent) {
        if !self.clipboard_local_on() {
            log::log_line("clipboard", "收到对方剪贴板内容，但本机未开启同步，已忽略");
            return;
        }
        if let ClipContent::Text(t) = &content {
            if t.len() > CLIPBOARD_MAX_BYTES {
                log::log_line(
                    "clipboard",
                    &format!("收到对方剪贴板文字 {} 字节，超过 2MB 上限，已忽略", t.len()),
                );
                return;
            }
        }
        let hash = content.hash();
        let kind = content.kind();
        let desc = match &content {
            ClipContent::Text(t) => format!("文字（{} 字节）", t.len()),
            ClipContent::Image { width, height, .. } => format!("图片（{width}x{height}）"),
        };
        let written = content.clone();
        let node = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            node.with_clipboard(|cb| clip_write_with(cb, &written))
                .unwrap_or_else(|| Err("系统剪贴板不可用".to_string()))
        })
        .await;
        match result {
            Ok(Ok(())) => {
                {
                    let mut g = self.clip_guard.lock().await;
                    g.suppress = Some(hash.clone());
                    g.last_seen = Some(hash);
                }
                log::log_line("clipboard", &format!("剪贴板同步：已写入对方{desc}"));
                self.emit(Event::ClipboardSynced {
                    kind: kind.to_string(),
                });
            }
            Ok(Err(e)) => {
                log::log_line("clipboard", &format!("写入本机剪贴板失败：{e}"));
            }
            Err(e) => {
                log::log_line("clipboard", &format!("写入本机剪贴板失败：{e}"));
            }
        }
    }

    // ---------- 文件传输 ----------
    /// 发送文件：流式分片、断点续传、选择性压缩。返回传输 ID。
    pub async fn send_file(&self, path: &Path) -> Result<String, ServiceError> {
        let session = self.session().await?;
        let meta = std::fs::metadata(path)?;
        let size = meta.len();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| ServiceError::Other("文件名无效".into()))?;
        let sha = sha256_file(path)?;
        let transfer_id = derive_transfer_id(&sha, &session.peer.device_id);
        let file_id = new_id();
        self.storage.save_file(&FileMeta {
            id: file_id.clone(),
            name: name.clone(),
            size,
            sha256: sha.clone(),
            path: path.to_string_lossy().to_string(),
            mime: None,
        })?;
        let msg = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::File,
            body: name.clone(),
            file_id: Some(file_id),
            reply_to: None,
            status: MessageStatus::Sending,
            created_at: now_ms(),
        };
        self.storage.insert_message(&msg)?;
        self.emit(Event::MessageNew(msg.clone()));

        // 等待对方的已收区间（续传）
        let (tx, rx) = oneshot::channel();
        self.accept_waiters
            .lock()
            .await
            .insert(transfer_id.clone(), tx);
        session
            .conn
            .send(&Frame::Control(Control::FileOffer {
                transfer_id: transfer_id.clone(),
                msg_id: msg.id.clone(),
                name: name.clone(),
                size,
                sha256: sha,
                mime: None,
            }))
            .await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| ServiceError::Other("等待对方接收确认超时".into()))?
            .map_err(|_| ServiceError::Other("对方接收通道已关闭".into()))?;
        if self.take_cancelled(&transfer_id).await {
            self.finish_cancelled(&transfer_id, &msg.id).await;
            return Ok(transfer_id);
        }
        let missing = missing_ranges(&received, size);
        self.emit(Event::TransferProgress {
            transfer_id: transfer_id.clone(),
            msg_id: msg.id.clone(),
            received: covered_bytes(&received),
            total: size,
        });

        // 按缺失区间流式发送
        let mut file = std::fs::File::open(path)?;
        let mut sent_total = covered_bytes(&received);
        for (start, end) in missing {
            let mut offset = start;
            file.seek(SeekFrom::Start(start))?;
            while offset < end {
                if self.take_cancelled(&transfer_id).await {
                    self.finish_cancelled(&transfer_id, &msg.id).await;
                    return Ok(transfer_id);
                }
                let want = ((end - offset) as usize).min(CHUNK_SIZE);
                let mut buf = vec![0u8; want];
                file.read_exact(&mut buf)?;
                let (compressed, data) = match try_compress(&buf, &name) {
                    Some(c) => (true, c),
                    None => (false, buf),
                };
                session
                    .conn
                    .send(&Frame::Chunk(FileChunk {
                        transfer_id: transfer_id.clone(),
                        offset,
                        compressed,
                        data,
                    }))
                    .await?;
                offset += want as u64;
                sent_total += want as u64;
                self.emit(Event::TransferProgress {
                    transfer_id: transfer_id.clone(),
                    msg_id: msg.id.clone(),
                    received: sent_total,
                    total: size,
                });
            }
        }
        // 等校验结果
        let (vtx, vrx) = oneshot::channel();
        self.verify_waiters
            .lock()
            .await
            .insert(transfer_id.clone(), vtx);
        session
            .conn
            .send(&Frame::Control(Control::FileDone {
                transfer_id: transfer_id.clone(),
            }))
            .await?;
        let ok = tokio::time::timeout(std::time::Duration::from_secs(60), vrx)
            .await
            .map_err(|_| ServiceError::Other("等待校验结果超时".into()))?
            .map_err(|_| ServiceError::Other("校验通道已关闭".into()))?;
        if ok {
            self.storage
                .update_message_status(&msg.id, MessageStatus::Delivered)?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "done".into(),
            });
        } else {
            self.storage
                .update_message_status(&msg.id, MessageStatus::Failed)?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "verify_failed".into(),
            });
        }
        Ok(transfer_id)
    }

    // 参数与 FileOffer 帧字段一一对应，豁免参数个数检查
    #[allow(clippy::too_many_arguments)]
    async fn on_file_offer(
        &self,
        session: &Arc<PeerSession>,
        transfer_id: String,
        msg_id: String,
        name: String,
        size: u64,
        sha256: String,
        _mime: Option<String>,
    ) -> Result<(), ServiceError> {
        // 恢复已有进度：传输记录 + .part 文件都在才算数
        let part_path = self.download_dir().join(format!("{transfer_id}.part"));
        let mut ranges = self
            .storage
            .load_transfer_ranges(&transfer_id)?
            .unwrap_or_default();
        if !part_path.exists() {
            ranges.clear();
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&part_path)?;
        file.set_len(size)?;
        let file_id = new_id();
        self.recv_files.lock().await.insert(
            transfer_id.clone(),
            RecvState {
                name: name.clone(),
                total: size,
                sha256: sha256.clone(),
                msg_id: msg_id.clone(),
                file_id: file_id.clone(),
                part_path,
                ranges: ranges.clone(),
                written: covered_bytes(&ranges),
            },
        );
        self.storage.save_file(&FileMeta {
            id: file_id.clone(),
            name: name.clone(),
            size,
            sha256,
            path: String::new(),
            mime: None,
        })?;
        let m = Message {
            id: msg_id,
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Peer,
            kind: MessageKind::File,
            body: name,
            file_id: Some(file_id),
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&m)?;
        self.emit(Event::MessageNew(m));
        self.storage.save_transfer(
            &transfer_id,
            "",
            "in",
            &session.peer.device_id,
            size,
            &ranges,
            "receiving",
        )?;
        session
            .conn
            .send(&Frame::Control(Control::FileAccept {
                transfer_id,
                received: ranges,
            }))
            .await?;
        Ok(())
    }

    async fn on_file_chunk(
        &self,
        _session: &Arc<PeerSession>,
        chunk: FileChunk,
    ) -> Result<(), ServiceError> {
        let mut guard = self.recv_files.lock().await;
        let st = guard
            .get_mut(&chunk.transfer_id)
            .ok_or_else(|| ServiceError::Other("收到未知传输的分片".into()))?;
        let data = if chunk.compressed {
            decompress(&chunk.data)?
        } else {
            chunk.data
        };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&st.part_path)?;
        f.seek(SeekFrom::Start(chunk.offset))?;
        f.write_all(&data)?;
        st.ranges = merge_ranges(
            [
                st.ranges.clone(),
                vec![(chunk.offset, chunk.offset + data.len() as u64)],
            ]
            .concat(),
        );
        st.written = covered_bytes(&st.ranges);
        let (tid, written, total, ranges, mid) = (
            chunk.transfer_id.clone(),
            st.written,
            st.total,
            st.ranges.clone(),
            st.msg_id.clone(),
        );
        drop(guard);
        self.storage
            .save_transfer(&tid, "", "in", "", total, &ranges, "receiving")?;
        self.emit(Event::TransferProgress {
            transfer_id: tid,
            msg_id: mid,
            received: written,
            total,
        });
        Ok(())
    }

    async fn on_file_done(
        &self,
        session: &Arc<PeerSession>,
        transfer_id: String,
    ) -> Result<(), ServiceError> {
        let st = self
            .recv_files
            .lock()
            .await
            .remove(&transfer_id)
            .ok_or_else(|| ServiceError::Other("完成信号对应未知传输".into()))?;
        let actual = sha256_file(&st.part_path)?;
        let ok = actual == st.sha256;
        if ok {
            let _ = self
                .storage
                .update_message_status(&st.msg_id, MessageStatus::Delivered);
            let final_path = self.download_dir().join(&st.name);
            std::fs::rename(&st.part_path, &final_path)?;
            // 回写落地路径，文件菜单（打开/另存为等）靠它定位文件
            let _ = self
                .storage
                .update_file_path(&st.file_id, &final_path.to_string_lossy());
            self.storage.save_transfer(
                &transfer_id,
                "",
                "in",
                &session.peer.device_id,
                st.total,
                &st.ranges,
                "done",
            )?;
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "done".into(),
            });
            log::log_line(
                "files",
                &format!(
                    "接收完成：{}（{} 字节）→ {}",
                    st.name,
                    st.total,
                    final_path.display()
                ),
            );
        } else {
            let _ = std::fs::remove_file(&st.part_path);
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "verify_failed".into(),
            });
            log::log_line("files", &format!("接收校验失败，已删除半成品：{}", st.name));
        }
        session
            .conn
            .send(&Frame::Control(Control::FileVerify { transfer_id, ok }))
            .await?;
        Ok(())
    }

    // ---------- 存储与文件 ----------
    /// 设置页存储区：聊天记录占用走数据库统计，文件/缓存走下载目录实测。
    pub fn storage_stats(&self) -> StorageStats {
        let (chat_count, chat_bytes) = self.storage.message_stats().unwrap_or((0, 0));
        let mut files_bytes = 0u64;
        let mut files_count = 0u64;
        let mut cache_bytes = 0u64;
        if let Ok(rd) = std::fs::read_dir(self.download_dir()) {
            for e in rd.flatten() {
                let Ok(md) = e.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                if e.file_name().to_string_lossy().ends_with(".part") {
                    cache_bytes += md.len();
                } else {
                    files_bytes += md.len();
                    files_count += 1;
                }
            }
        }
        StorageStats {
            chat_count,
            chat_bytes,
            files_bytes,
            files_count,
            cache_bytes,
        }
    }

    /// 清理缓存：删掉已不在接收中的 .part 半成品文件，返回释放字节数。
    pub async fn clean_cache(&self) -> u64 {
        let active: HashSet<String> = self.recv_files.lock().await.keys().cloned().collect();
        let mut freed = 0u64;
        if let Ok(rd) = std::fs::read_dir(self.download_dir()) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(tid) = name.strip_suffix(".part") else {
                    continue;
                };
                if active.contains(tid) {
                    continue;
                }
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(e.path()).is_ok() {
                    freed += size;
                }
            }
        }
        freed
    }

    /// 清空全部聊天记录（设置页存储区，二次确认后调用）。
    pub fn clear_chat_history(&self) -> Result<(), ServiceError> {
        self.storage.clear_all_messages()?;
        Ok(())
    }

    /// 一条文件消息对应的本地文件信息（文件菜单用）：路径优先取记录值，
    /// 接收文件记录缺失时按下载目录 + 文件名兜底定位。
    pub fn file_info(&self, message_id: &str) -> Result<Option<MessageFileInfo>, ServiceError> {
        let Some(m) = self.storage.get_message(message_id)? else {
            return Ok(None);
        };
        let Some(file_id) = m.file_id else {
            return Ok(None);
        };
        let Some(meta) = self.storage.get_file(&file_id)? else {
            return Ok(None);
        };
        let recorded = PathBuf::from(&meta.path);
        let path = if !meta.path.is_empty() && recorded.exists() {
            Some(recorded)
        } else {
            let fallback = self.download_dir().join(&meta.name);
            if fallback.exists() {
                Some(fallback)
            } else if !meta.path.is_empty() {
                Some(recorded)
            } else {
                None
            }
        };
        let exists = path.as_ref().map(|p| p.exists()).unwrap_or(false);
        Ok(Some(MessageFileInfo {
            name: meta.name,
            size: meta.size,
            path: path.map(|p| p.to_string_lossy().to_string()),
            exists,
        }))
    }

    // ---------- AI ----------
    /// 确保存在 AI 会话，返回会话 ID。
    pub fn ensure_ai_conversation(&self) -> Result<String, ServiceError> {
        let id = "ai-default".to_string();
        self.storage.create_conversation(&Conversation {
            id: id.clone(),
            kind: ConversationKind::Ai,
            peer_device_id: None,
            title: "AI 助手".into(),
            draft: String::new(),
            last_read_id: None,
            created_at: now_ms(),
        })?;
        Ok(id)
    }

    /// 在 AI 会话里提问：走「默认 AI」来源（本机配置 / 连接设备的 AI / 自动）。
    pub async fn ai_send(self: &Arc<Self>, text: &str) -> Result<Message, ServiceError> {
        let conv_id = self.ensure_ai_conversation()?;
        self.ai_push_and_answer(&conv_id, text).await
    }

    /// 聊天页「问AI」：把某条聊天消息作为上下文发给默认 AI，问答落在 AI 会话里
    /// （用户消息带出处标题，方便在 AI 页接着追问）。
    pub async fn ai_ask_message(
        self: &Arc<Self>,
        conv_id: &str,
        msg_id: &str,
    ) -> Result<Message, ServiceError> {
        let orig = self
            .storage
            .get_message(msg_id)?
            .ok_or_else(|| ServiceError::Other("要问 AI 的消息不存在".into()))?;
        let title = self
            .storage
            .list_conversations()?
            .into_iter()
            .find(|c| c.id == conv_id)
            .map(|c| c.title)
            .unwrap_or_else(|| "聊天".into());
        let ai_conv = self.ensure_ai_conversation()?;
        let body = format!("【来自与「{title}」的聊天】\n{}", orig.body);
        self.ai_push_and_answer(&ai_conv, &body).await
    }

    /// 聊天框 @AI 提问：问题与回答都落在当前聊天会话里（仅本机保存，不发给对方），
    /// 来源由前端 @ 选择决定（本机 / 对方设备 / 默认）。
    pub async fn chat_ask_ai(
        self: &Arc<Self>,
        conv_id: &str,
        source: AiSource,
        text: &str,
    ) -> Result<Message, ServiceError> {
        let q = Message {
            id: new_id(),
            conversation_id: conv_id.to_string(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: now_ms(),
        };
        let qid = q.id.clone();
        self.storage.insert_message(&q)?;
        self.emit(Event::MessageNew(q));
        // @ 选择「默认」时按设置里的默认来源走（未设置则自动）
        let source = if source == AiSource::Auto {
            self.default_ai_source()
        } else {
            source
        };
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: text.to_string(),
        }];
        let full = self.chat_core_src(&msgs, None, source).await?;
        let a = Message {
            id: new_id(),
            conversation_id: conv_id.to_string(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: Some(qid),
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&a)?;
        self.emit(Event::MessageNew(a.clone()));
        Ok(a)
    }

    /// 把一条用户消息写入 AI 会话并生成回复（AI 页提问与「问AI」共用）。
    async fn ai_push_and_answer(
        self: &Arc<Self>,
        conv_id: &str,
        text: &str,
    ) -> Result<Message, ServiceError> {
        let conv_id = conv_id.to_string();
        let user_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: now_ms(),
        };
        self.storage.insert_message(&user_msg)?;
        self.emit(Event::MessageNew(user_msg));

        // 组装上下文（最近 20 条）
        let history = self.storage.list_messages(&conv_id, None, 20)?;
        let msgs: Vec<ChatMessage> = history
            .iter()
            .map(|m| ChatMessage {
                role: if m.sender == Sender::Ai {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: m.body.clone(),
            })
            .collect();

        let full = self.chat_anywhere(&conv_id, &msgs).await?;

        let ai_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&ai_msg)?;
        self.emit(Event::AiDone {
            conversation_id: conv_id,
            message: ai_msg.clone(),
        });
        Ok(ai_msg)
    }

    /// 重新生成最后一条 AI 回复：删掉旧回复，用同一上下文再生成一条。
    pub async fn ai_regenerate(self: &Arc<Self>) -> Result<Message, ServiceError> {
        let conv_id = self.ensure_ai_conversation()?;
        let history = self.storage.list_messages(&conv_id, None, 50)?;
        // 找到并删除最后一条 AI 回复
        if let Some(last_ai) = history.iter().rev().find(|m| m.sender == Sender::Ai) {
            self.storage.delete_message(&last_ai.id)?;
        }
        let history = self.storage.list_messages(&conv_id, None, 20)?;
        if !history.iter().any(|m| m.sender == Sender::Me) {
            return Err(ServiceError::Other("还没有提问过，无法重新生成".into()));
        }
        let msgs: Vec<ChatMessage> = history
            .iter()
            .map(|m| ChatMessage {
                role: if m.sender == Sender::Ai {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: m.body.clone(),
            })
            .collect();
        let full = self.chat_anywhere(&conv_id, &msgs).await?;
        let ai_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
        };
        self.storage.insert_message(&ai_msg)?;
        self.emit(Event::AiDone {
            conversation_id: conv_id,
            message: ai_msg.clone(),
        });
        Ok(ai_msg)
    }

    /// AI 调用核心：本机配置优先（按配置的流式开关走流式/整段），
    /// 否则经已连接设备代理转发；两条路的增量都交给 on_delta（可为空）。
    /// 默认 AI 来源（用户 2026-11-18 要求：默认 AI 可选本机配置或连接设备的 AI）。
    /// 设置键 ai_default_source = local / peer；未设置 = Auto（本机优先、否则经对端转发）。
    fn default_ai_source(&self) -> AiSource {
        match self
            .storage
            .get_setting("ai_default_source")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("local") => AiSource::Local,
            Some("peer") => AiSource::Peer,
            _ => AiSource::Auto,
        }
    }

    async fn chat_core(
        &self,
        msgs: &[ChatMessage],
        on_delta: Option<&mut (dyn FnMut(&str) + Send)>,
    ) -> Result<String, ServiceError> {
        self.chat_core_src(msgs, on_delta, AiSource::Auto).await
    }

    /// 按指定来源提问：Local 只走本机配置、Peer 只走对端转发、Auto 本机优先否则转发。
    async fn chat_core_src(
        &self,
        msgs: &[ChatMessage],
        on_delta: Option<&mut (dyn FnMut(&str) + Send)>,
        source: AiSource,
    ) -> Result<String, ServiceError> {
        let cfg = self.ai_cfg.lock().await.clone();
        if source != AiSource::Peer {
            if let Some(cfg) = cfg.filter(|c| c.is_ready()) {
                let client = AiClient::new(cfg.clone());
                if cfg.stream {
                    let mut cb = on_delta;
                    let full = client
                        .chat_stream(msgs, |d| {
                            if let Some(f) = cb.as_deref_mut() {
                                f(d);
                            }
                        })
                        .await?;
                    return Ok(full);
                }
                let full = client.chat_once(msgs).await?;
                if let Some(f) = on_delta {
                    f(&full);
                }
                return Ok(full);
            }
            if source == AiSource::Local {
                return Err(bluelink_ai::AiError::NotConfigured.into());
            }
        }
        if source != AiSource::Local {
            if self.is_connected().await {
                return self.proxy_to_peer(msgs, on_delta).await;
            }
            if source == AiSource::Peer {
                return Err(ServiceError::NotConnected);
            }
        }
        Err(bluelink_ai::AiError::NotConfigured.into())
    }

    /// AI 页的一轮调用：增量同步转成 AiChunk 事件推给前端。
    async fn chat_anywhere(
        self: &Arc<Self>,
        conv_id: &str,
        msgs: &[ChatMessage],
    ) -> Result<String, ServiceError> {
        let node = self.clone();
        let cid = conv_id.to_string();
        let mut cb = move |d: &str| {
            node.emit(Event::AiChunk {
                conversation_id: cid.clone(),
                delta: d.to_string(),
            });
        };
        // AI 页提问遵循「默认 AI」设置（本机 / 对方设备 / 自动）
        let source = self.default_ai_source();
        self.chat_core_src(msgs, Some(&mut cb), source).await
    }

    /// 本地 API 网关入口：供本机其他软件经 OpenAI 兼容接口调用（见应用层本地 API 服务）。
    /// 路由与 AI 页一致：本机配置优先，否则经已连接设备转发；请求里带的模型名仅作参考，
    /// 实际模型以真正调用上游一侧的配置为准（/v1/models 如实返回该模型名）。
    pub async fn gateway_chat(
        &self,
        msgs: &[ChatMessage],
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String, ServiceError> {
        self.chat_core(msgs, Some(on_delta)).await
    }

    /// 本机当前生效的 AI 模型名（本地接口 /v1/models 用）：本机配置优先，
    /// 未配置时返回网关占位名（经对端转发时实际模型在对端）。
    pub async fn gateway_model(&self) -> String {
        let cfg = self.ai_cfg.lock().await.clone();
        match cfg.filter(|c| c.is_ready()) {
            Some(c) => c.model,
            None => "bluelink-proxy".to_string(),
        }
    }

    /// 经对端代理提问：响应帧由 handle_ai_proxy_frame 转入等待通道，增量同步回调。
    async fn proxy_to_peer(
        &self,
        msgs: &[ChatMessage],
        mut on_delta: Option<&mut (dyn FnMut(&str) + Send)>,
    ) -> Result<String, ServiceError> {
        let session = self.session().await?;
        let request_id = new_id();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<String, String>>(64);
        self.proxy_waiters
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let payload: Vec<serde_json::Value> = msgs
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();
        session
            .conn
            .send(&Frame::Control(Control::AiProxyRequest {
                request_id: request_id.clone(),
                messages: payload,
                model: None,
            }))
            .await?;
        let mut full = String::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => {
                    if delta.is_empty() {
                        break;
                    } // 空增量 = 结束信号
                    if let Some(f) = on_delta.as_deref_mut() {
                        f(&delta);
                    }
                    full.push_str(&delta);
                }
                Err(e) => {
                    self.proxy_waiters.lock().await.remove(&request_id);
                    return Err(ServiceError::Other(e));
                }
            }
        }
        self.proxy_waiters.lock().await.remove(&request_id);
        if full.is_empty() {
            return Err(ServiceError::Other("对方未能提供 AI 代理".into()));
        }
        Ok(full)
    }

    async fn handle_ai_proxy_frame(&self, c: Control) {
        match c {
            Control::AiProxyChunk { request_id, delta } => {
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx.send(Ok(delta)).await;
                }
            }
            Control::AiProxyDone { request_id, error } => {
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx.send(error.map(Err).unwrap_or(Ok(String::new()))).await;
                }
            }
            _ => {}
        }
    }

    /// 今日转发统计的存储键：按 UTC+8 日期分桶（本应用以国内时区为准）。
    fn proxy_day_key() -> String {
        let days = (now_ms() + 8 * 3600 * 1000) / 86_400_000;
        format!("proxy_day_{days}")
    }

    /// 今日已转发统计（次数, 约 tokens；tokens 按字符数粗估，中文约 1 字 1 token）。
    pub fn proxy_today_stats(&self) -> (u64, u64) {
        let raw = self
            .storage
            .get_setting(&Self::proxy_day_key())
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut it = raw.split(',');
        let n = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let c = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (n, c)
    }

    fn add_proxy_stats(&self, chars: u64) {
        let (n, c) = self.proxy_today_stats();
        let _ = self
            .storage
            .set_setting(&Self::proxy_day_key(), &format!("{},{}", n + 1, c + chars));
    }

    /// 宿主侧：替无网对端调用本机 AI 并流式回传。
    /// 前置门槛：本机用户须在设置里开启「AI 转发」许可（默认关）——花的是本机密钥额度；
    /// 未开启时明确回绝，让对方界面能如实提示。
    async fn serve_ai_proxy(
        &self,
        session: Arc<PeerSession>,
        request_id: String,
        messages: Vec<serde_json::Value>,
    ) -> Result<(), ServiceError> {
        let allowed = self
            .storage
            .get_setting("ai_proxy_allow")
            .map(|v| v.as_deref() == Some("1"))
            .unwrap_or(false);
        if !allowed {
            session
                .conn
                .send(&Frame::Control(Control::AiProxyDone {
                    request_id,
                    error: Some(
                        "对方未开启 AI 转发许可：请让对方在设置的「AI 转发」里打开允许开关".into(),
                    ),
                }))
                .await?;
            return Ok(());
        }
        let cfg = self.ai_cfg.lock().await.clone();
        let Some(cfg) = cfg.filter(|c| c.is_ready()) else {
            session
                .conn
                .send(&Frame::Control(Control::AiProxyDone {
                    request_id,
                    error: Some("对方未配置 AI".into()),
                }))
                .await?;
            return Ok(());
        };
        let msgs: Vec<ChatMessage> = messages
            .iter()
            .map(|v| ChatMessage {
                role: v
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("user")
                    .to_string(),
                content: v
                    .get("content")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();
        let prompt_chars: u64 = msgs.iter().map(|m| m.content.chars().count() as u64).sum();
        let client = AiClient::new(cfg);
        // 增量先入通道、由转发任务顺序发出，保证分片先于 Done 到达（不能逐段 spawn，会乱序）
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let conn_fwd = session.conn.clone();
        let rid_fwd = request_id.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                let _ = conn_fwd
                    .send(&Frame::Control(Control::AiProxyChunk {
                        request_id: rid_fwd.clone(),
                        delta: d,
                    }))
                    .await;
            }
        });
        let result = client
            .chat_stream(&msgs, |delta| {
                let _ = tx.try_send(delta.to_string());
            })
            .await;
        drop(tx);
        let _ = forwarder.await;
        let error = match &result {
            Ok(full) => {
                self.add_proxy_stats(prompt_chars + full.chars().count() as u64);
                None
            }
            Err(e) => Some(e.to_string()),
        };
        session
            .conn
            .send(&Frame::Control(Control::AiProxyDone { request_id, error }))
            .await?;
        Ok(())
    }
}
