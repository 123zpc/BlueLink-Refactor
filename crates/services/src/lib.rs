//! 应用服务层：把协议、传输、存储、AI 编排成一个可用的节点（Node）。
//! 桌面端（Tauri）与演示 CLI 都只调用这一层，不直接碰底层 crate。

pub mod log;
mod machine_id;
pub use log::log_line;

use bluelink_ai::{AiClient, AiConfig, ChatMessage};
use bluelink_domain::*;
use bluelink_protocol::{
    ClipboardImage, Control, FileChunk, Frame, GroupRef, CHUNK_SIZE, COMP_NONE, COMP_ZLIB_LEGACY,
    COMP_ZSTD,
};
use bluelink_storage::Storage;
use bluelink_transport::Conn;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// 已有消息的内容发生变化（图片组接收端逐张追加清单时用）：
    /// 前端按 id 找到已渲染行并重建该行，位置（seq）不变。
    MessageUpdated(Message),
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

/// 当前分片压缩算法名（发送完成日志用；与协议层压缩标志对应）。
const COMPRESSION_LABEL: &str = "zstd";
/// zstd 压缩级别：3 级是速度/压缩率的平衡点（空中带宽才是瓶颈，
/// 不用高级别拖慢 CPU）。
const ZSTD_LEVEL: i32 = 3;

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
/// run_file_transfer 的结果：Done=走完校验（ok 为校验结论，附空中
/// 字节/是否压缩/起始时刻供日志），Cancelled=被取消（收尾已在内部完成）。
enum TransferRun {
    Done {
        ok: bool,
        wire_bytes: u64,
        used_compression: bool,
        started: std::time::Instant,
    },
    Cancelled,
}

pub fn derive_transfer_id(file_sha: &str, peer_id: &str) -> String {
    let d = Sha256::digest(format!("{file_sha}:{peer_id}").as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
    )
}

/// 尝试压缩分片（zstd）：已知压缩格式或压不动时返回 None（直传）。
fn try_compress(data: &[u8], file_name: &str) -> Option<Vec<u8>> {
    let ext = Path::new(file_name).extension()?.to_str()?.to_lowercase();
    if KNOWN_COMPRESSED.contains(&ext.as_str()) {
        return None;
    }
    let out = zstd::bulk::compress(data, ZSTD_LEVEL).ok()?;
    if out.len() < data.len() {
        Some(out)
    } else {
        None
    }
}

/// 按压缩标志解压分片。旧版 zlib 标志与未知算法都明确报错、干净失败：
/// 半成品文件不会通过 FileVerify 的 SHA-256 校验，绝不写出坏文件。
fn decompress(data: &[u8], compression: u8) -> Result<Vec<u8>, ServiceError> {
    match compression {
        COMP_ZSTD => {
            use std::io::Read as _;
            let mut dec = zstd::stream::read::Decoder::new(data)
                .map_err(|e| ServiceError::Other(format!("zstd 解压失败：{e}")))?;
            // 单个分片解压后至多 CHUNK_SIZE 字节，留一倍余量做安全上限
            let mut out = Vec::new();
            (&mut dec)
                .take((CHUNK_SIZE * 2) as u64)
                .read_to_end(&mut out)
                .map_err(|e| ServiceError::Other(format!("zstd 解压失败：{e}")))?;
            if out.len() > CHUNK_SIZE {
                return Err(ServiceError::Other("分片解压后大小异常，已停止传输".into()));
            }
            Ok(out)
        }
        COMP_ZLIB_LEGACY => Err(ServiceError::Other(
            "对方版本较旧（使用旧版 zlib 压缩），请对方升级到 v2.5.2 以上后再传此类文件".into(),
        )),
        other => Err(ServiceError::Other(format!(
            "不认识的压缩算法（标志 {other}），已停止本次传输"
        ))),
    }
}

/// 无条件 zstd 压缩（剪贴板图片用；压不动时调用方自行回落直传）。
fn zstd_compress(data: &[u8]) -> Option<Vec<u8>> {
    zstd::bulk::compress(data, ZSTD_LEVEL).ok()
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
    /// 增量校验状态：收片时顺手把数据喂给哈希器，FileDone 时免整文件
    /// 重读。hashed_upto 是已按序喂入的字节数（文件前缀长度）；只有
    /// 全新传输（无续传区间）才启用，续传/异常时置 None 回退整文件
    /// 重算，校验强度不降。
    hasher: Option<Sha256>,
    hashed_upto: u64,
}

/// 一个对端会话。
struct PeerSession {
    conn: Arc<Conn>,
    peer: Device,
    conversation_id: String,
    /// 最近一次收到入站帧的时刻（毫秒）：心跳任务据此判死——链路静默
    /// 死亡（对端进程没了/无线电被关/会话被对端静默丢弃）时收发两端都
    /// 不会报错，只有「太久没有任何入站帧」是可靠信号（2026-11-20
    /// 双机日志实锤：一边会话已死、另一边 28 秒后才靠人工断开察觉）。
    last_inbound_ms: AtomicU64,
}

/// 心跳节奏：每 5 秒发一帧 Ping（协议控制帧，对端现成回 Pong，任何
/// 入站帧都刷新存活戳）；连续 15 秒无任何入站帧判死。两边同跑这套，
/// 静默死亡的察觉差从「无上限」压到 15 秒内。
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const HEARTBEAT_DEAD_MS: u64 = 15_000;

/// 节点：本机身份 + 存储 + 当前连接 + AI 配置。
pub struct Node {
    pub device_id: String,
    pub device_name: String,
    pub storage: Arc<Storage>,
    /// 接收文件保存目录：可在设置里修改（运行时可换，存在 RwLock 里）。
    download_dir: std::sync::RwLock<PathBuf>,
    /// 默认保存目录（数据目录下的 downloads），「恢复默认」时回到这里。
    default_download_dir: PathBuf,
    /// 默认日志目录（数据目录下的 logs），日志目录「恢复默认」时回到这里。
    default_log_dir: PathBuf,
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
        log::install_panic_hook();
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
        // 日志规矩（v2.5.1）：启动时应用已保存的日志目录与日志级别——
        // dev_mode 开 = 全量日志；运行中切换在 set_dev_mode 命令侧即时调 set_verbose。
        let default_log_dir = data_dir.join("logs");
        if let Some(dir) = storage
            .get_setting("log_dir")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
        {
            log::set_log_dir(std::path::Path::new(&dir));
        }
        log::set_verbose(storage.get_setting("dev_mode").ok().flatten().as_deref() == Some("1"));
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
        // 设备 ID（2026-11-20 改机器绑定）：优先按操作系统机器标识
        // 确定性派生（见 machine_bound_device_id），卸载重装/清数据后
        // 不变；派生值仍写进 device_id.txt——机器标识偶发读不到时靠它
        // 兜底沿用。存量随机 ID 与派生值不同则一次性切换并落定案行。
        // 联调覆盖：环境变量 BLUELINK_DEVICE_ID_OVERRIDE 非空时直接用它
        // （同一台机器上跑双实例联调时区分身份用；设备 ID 不是秘密，
        // 连接码本就显示在界面上，此覆盖不构成安全边界）。
        let id_path = data_dir.join("device_id.txt");
        let stored_id = std::fs::read_to_string(&id_path)
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());
        let env_override = std::env::var("BLUELINK_DEVICE_ID_OVERRIDE")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());
        let device_id = if let Some(id) = env_override {
            let _ = std::fs::write(&id_path, &id);
            id
        } else {
            match (machine_id::read_machine_id(), stored_id) {
                (Some(mid), stored) => {
                    let derived = machine_bound_device_id(&mid);
                    match stored {
                        Some(s) if s == derived => derived,
                        Some(_) => {
                            let _ = std::fs::write(&id_path, &derived);
                            log::log_line(
                                "app",
                                "设备 ID 已切换为机器绑定，连接码随之变更（仅此一次）",
                            );
                            derived
                        }
                        None => {
                            let _ = std::fs::write(&id_path, &derived);
                            log::log_line("app", "设备 ID 已按机器标识绑定生成（重装/清数据不变）");
                            derived
                        }
                    }
                }
                (None, Some(s)) => {
                    log::log_line("app", "机器标识读取失败，沿用已持久化的设备 ID");
                    s
                }
                (None, None) => {
                    let id = new_id();
                    let _ = std::fs::write(&id_path, &id);
                    log::log_line(
                        "app",
                        "机器标识读取失败，设备 ID 随机生成并持久化（清数据后会变）",
                    );
                    id
                }
            }
        };
        let (events, _) = broadcast::channel(512);
        Ok(Arc::new(Self {
            device_id,
            device_name: device_name.to_string(),
            storage,
            download_dir: std::sync::RwLock::new(download_dir),
            default_download_dir,
            default_log_dir,
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

    /// 修改日志目录：传 None 恢复默认（<数据目录>/logs）。立即切换写入目标
    /// 并持久化；旧目录的日志文件不搬不删，只是不再往那里写。
    pub fn set_log_dir(&self, dir: Option<&Path>) -> Result<PathBuf, ServiceError> {
        let target = match dir {
            Some(p) => p.to_path_buf(),
            None => self.default_log_dir.clone(),
        };
        log::set_log_dir(&target)
            .ok_or_else(|| ServiceError::Other("日志目录不可用，切换失败".into()))?;
        let saved = if dir.is_some() {
            target.to_string_lossy().to_string()
        } else {
            String::new()
        };
        self.storage.set_setting("log_dir", &saved)?;
        log::log_line("app", &format!("日志目录改为 {}", target.display()));
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
            log::log_verbose("conn", "握手步骤5/6：等待对方 Hello");
            match conn.recv().await? {
                Frame::Control(Control::Hello {
                    device_id,
                    device_name,
                    code,
                    ..
                }) => {
                    log::log_verbose(
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
                    log::log_verbose(
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
            log::log_verbose(
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
                    log::log_verbose(
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
        .map_err(|_| {
            // 超时定案行必须落日志（essential）：2026-11-20 真机实证——对端
            // HelloAck 没送达时发起侧干等到超时，旧版只把错误回给界面，
            // 日志里无声无息，排查无从下手。
            log::log_line(
                "conn",
                "连接失败于步骤5/6：握手超时（20 秒内未完成 Hello/HelloAck 往返）",
            );
            ServiceError::Other("握手超时：对方未在 20 秒内响应".into())
        })??;

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
            last_inbound_ms: AtomicU64::new(now_ms() as u64),
        });
        // 已有旧会话时先按「被新连接替换」收尾再换上新会话，不许无声覆盖
        let old = self.peer.lock().await.clone();
        if let Some(old) = old {
            self.end_session(&old, "被新连接替换").await;
        }
        *self.peer.lock().await = Some(session.clone());
        // 连接结果是 essential：普通模式也要能查到「连上了谁」（握手分步行是 verbose，不能替代结果行）
        log::log_line(
            "conn",
            &format!(
                "连接已建立：对方 {}（{}）",
                session.peer.name, session.peer.device_id
            ),
        );
        self.emit(Event::ConnectionChanged {
            connected: true,
            peer_name: Some(session.peer.name.clone()),
        });

        // 接收循环 + 心跳（判死察觉，见 spawn_heartbeat）
        let peer_out = session.peer.clone();
        let node = self.clone();
        let recv_session = session.clone();
        tokio::spawn(async move { node.recv_loop(recv_session).await });
        self.spawn_heartbeat(session.clone());

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

    /// 断开当前连接（界面按钮/取消清理都走这里）：经 end_session 以
    /// 「用户主动断开」落字。只清指针不关连接的话，对端会一直以为还
    /// 连着（2026-11-19 取消迟到成功实测撞出），故收尾必关底层连接。
    pub async fn disconnect(&self) {
        let session = self.peer.lock().await.clone();
        match session {
            Some(s) => self.end_session(&s, "用户主动断开").await,
            None => {
                // 无会话时也广播一次断开事件，前端状态归零（旧行为）
                self.emit(Event::ConnectionChanged {
                    connected: false,
                    peer_name: None,
                });
                self.emit_clipboard_state(false);
            }
        }
    }

    /// 会话结束的唯一出口：清会话槽、落 essential 定案行、发断开事件、
    /// 关底层连接。reason 必须是真实来源（用户主动断开 / 链路结束 /
    /// 接收出错：… / 心跳判死：… / 发送失败：… / 被新连接替换）——
    /// 2026-11-20 双机日志实锤旧版把清理动作统一打成「本机主动」，
    /// 与对端先察觉的事实矛盾、误导排查，从此分开落字。多个结束路径
    /// （接收循环/心跳/界面断开/新连接替换）赛跑时只有先到者执行：
    /// 同一会话只落一行、只发一次事件。
    async fn end_session(&self, session: &Arc<PeerSession>, reason: &str) {
        {
            let mut guard = self.peer.lock().await;
            let is_current = guard
                .as_ref()
                .map(|cur| Arc::ptr_eq(cur, session))
                .unwrap_or(false);
            if !is_current {
                return; // 已被其他路径结束或替换
            }
            *guard = None;
        }
        self.clip_peer_on.store(false, Ordering::Relaxed);
        log::log_line(
            "conn",
            &format!("连接已断开（{reason}）：对方 {}", session.peer.name),
        );
        self.emit(Event::ConnectionChanged {
            connected: false,
            peer_name: None,
        });
        self.emit_clipboard_state(false);
        session.conn.close().await;
    }

    /// 心跳任务（每会话一个）：周期发 Ping 探活；连续 HEARTBEAT_DEAD_MS
    /// 无任何入站帧判死并经 end_session 收尾。会话被替换/结束后自行
    /// 退出。两边同跑：静默死亡（对端会话被静默丢弃、进程消失、无线
    /// 电被关）不再靠人工察觉，察觉差压进判死阈值内。
    fn spawn_heartbeat(self: &Arc<Self>, session: Arc<PeerSession>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            tick.tick().await; // 跳过 interval 的立即第一拍
            loop {
                tick.tick().await;
                {
                    let guard = node.peer.lock().await;
                    let is_current = guard
                        .as_ref()
                        .map(|cur| Arc::ptr_eq(cur, &session))
                        .unwrap_or(false);
                    if !is_current {
                        return; // 会话已结束/被替换
                    }
                }
                let silent_ms = now_ms() - session.last_inbound_ms.load(Ordering::Relaxed) as i64;
                if silent_ms >= HEARTBEAT_DEAD_MS as i64 {
                    // 传输进行中不判死：发完等校验/收片未齐时，对端是
                    // 在合法地等我方（校验重算可达数秒），静默不等于
                    // 死亡——v2.5.2 实测撞出：300MB 文件校验期接收端
                    // 被自家心跳误杀。真死由传输侧超时（30s/60s）与
                    // 接收循环错误兜底。
                    let busy = !node.recv_files.lock().await.is_empty()
                        || !node.verify_waiters.lock().await.is_empty()
                        || !node.accept_waiters.lock().await.is_empty();
                    if !busy {
                        node.end_session(
                            &session,
                            &format!("心跳判死：{} 秒无任何入站帧", silent_ms / 1000),
                        )
                        .await;
                        return;
                    }
                }
                if let Err(e) = session.conn.send(&Frame::Control(Control::Ping)).await {
                    node.end_session(&session, &format!("发送失败：心跳帧发送出错（{e}）"))
                        .await;
                    return;
                }
            }
        });
    }

    async fn recv_loop(self: Arc<Self>, session: Arc<PeerSession>) {
        // 结束原因分开落字：干净 EOF（对端关连接/链路被栈断开）与接收
        // 出错（帧失步/IO 错误）不是一回事；具体传输侧原因看相邻的
        // transport essential 行（通知流结束/写失败/取消订阅等）。
        let reason = loop {
            match session.conn.recv().await {
                Ok(frame) => {
                    session
                        .last_inbound_ms
                        .store(now_ms() as u64, Ordering::Relaxed);
                    if let Err(e) = self.handle_frame(&session, frame).await {
                        eprintln!("处理帧出错：{e}");
                    }
                }
                Err(bluelink_transport::TransportError::Closed) => {
                    break "链路结束".to_string();
                }
                Err(e) => break format!("接收出错：{e}"),
            }
        };
        self.end_session(&session, &reason).await;
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
                let mut m = Message {
                    id: msg_id.clone(),
                    conversation_id: session.conversation_id.clone(),
                    sender: Sender::Peer,
                    kind: MessageKind::Text,
                    body: text,
                    file_id: None,
                    reply_to,
                    status: MessageStatus::Received,
                    created_at: ts,
                    seq: 0,
                };
                m.seq = self.storage.insert_message(&m)?;
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
                group,
            }) => {
                self.on_file_offer(
                    session,
                    transfer_id,
                    msg_id,
                    name,
                    size,
                    sha256,
                    mime,
                    group,
                )
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
        let mut m = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: reply_to.clone(),
            status: MessageStatus::Sending,
            created_at: now_ms(),
            seq: 0,
        };
        m.seq = self.storage.insert_message(&m)?;
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
            MessageKind::ImageGroup => {
                // 组重发：按清单收集仍在原位的成员文件，整组重新发出
                // （生成新组消息，与文件重发的口径一致）；只剩一张时
                // 退化为普通文件发送；一张都不在则明确报错。
                let mut paths = Vec::new();
                for it in parse_group_manifest(&m.body) {
                    if let Some(meta) = self.storage.get_file(&it.file_id)? {
                        let p = PathBuf::from(&meta.path);
                        if !meta.path.is_empty() && p.exists() {
                            paths.push(p);
                        }
                    }
                }
                if paths.is_empty() {
                    return Err(ServiceError::Other("原图片已不存在，无法重发".into()));
                }
                let node = self.clone();
                tokio::spawn(async move {
                    let _ = if paths.len() == 1 {
                        node.send_file(&paths[0]).await.map(|_| ())
                    } else {
                        node.send_image_group(&paths).await.map(|_| ())
                    };
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
                    log::log_verbose("clipboard", &format!("剪贴板同步：已发送文字（{n} 字节）"));
                }
            }
            ClipContent::Image {
                width,
                height,
                rgba,
            } => {
                // 位图先 zstd 压缩（截图类内容压缩率高），上限按实际发送大小算
                let digest = clip_image_digest(width, height, &rgba);
                let compressed = zstd_compress(&rgba).filter(|c| c.len() < rgba.len());
                let (data, comp_algo) = match &compressed {
                    Some(c) => (c.clone(), COMP_ZSTD),
                    None => (rgba, COMP_NONE),
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
                    compression: comp_algo,
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
            compression,
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
        let rgba = if compression == COMP_ZLIB_LEGACY {
            // 旧版 zlib 图片：对方版本较旧，本版不再解，忽略并在日志注明
            log::log_line(
                "clipboard",
                "收到对方剪贴板图片（旧版 zlib 压缩），对方版本较旧，已忽略",
            );
            return;
        } else if compression == COMP_ZSTD {
            match tokio::task::spawn_blocking(move || {
                use std::io::Read as _;
                let mut dec = zstd::stream::read::Decoder::new(&data[..])?;
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
        } else if compression == COMP_NONE {
            data
        } else {
            log::log_line(
                "clipboard",
                &format!("收到对方剪贴板图片，不认识的压缩算法（标志 {compression}），已忽略"),
            );
            return;
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
            log::log_verbose("clipboard", "收到对方剪贴板内容，但本机未开启同步，已忽略");
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
                log::log_verbose("clipboard", &format!("剪贴板同步：已写入对方{desc}"));
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
        let mut msg = Message {
            id: new_id(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::File,
            body: name.clone(),
            file_id: Some(file_id),
            reply_to: None,
            status: MessageStatus::Sending,
            created_at: now_ms(),
            seq: 0,
        };
        msg.seq = self.storage.insert_message(&msg)?;
        self.emit(Event::MessageNew(msg.clone()));

        match self
            .run_file_transfer(
                &session,
                path,
                name.clone(),
                size,
                sha.clone(),
                transfer_id.clone(),
                msg.id.clone(),
                None,
            )
            .await?
        {
            TransferRun::Cancelled => Ok(transfer_id),
            TransferRun::Done {
                ok,
                wire_bytes,
                used_compression,
                started,
            } => {
                if ok {
                    log::log_line(
                        "files",
                        &format!(
                            "文件发送完成：{name}，原始 {size} 字节，空中 {wire_bytes} 字节，耗时 {} 毫秒，压缩 {}",
                            started.elapsed().as_millis(),
                            if used_compression { COMPRESSION_LABEL } else { "无" },
                        ),
                    );
                    self.storage
                        .update_message_status(&msg.id, MessageStatus::Delivered)?;
                    // 状态事件补发：文件消息没有 Delivered 控制帧回执，
                    // 不发事件前端气泡会一直停在「发送中」（v2.5.2 前的老缺口）
                    self.emit(Event::MessageStatus {
                        id: msg.id.clone(),
                        status: MessageStatus::Delivered,
                    });
                    self.emit(Event::TransferState {
                        transfer_id: transfer_id.clone(),
                        state: "done".into(),
                    });
                } else {
                    self.storage
                        .update_message_status(&msg.id, MessageStatus::Failed)?;
                    self.emit(Event::MessageStatus {
                        id: msg.id.clone(),
                        status: MessageStatus::Failed,
                    });
                    self.emit(Event::TransferState {
                        transfer_id: transfer_id.clone(),
                        state: "verify_failed".into(),
                    });
                }
                Ok(transfer_id)
            }
        }
    }

    /// 单个文件的线上发送主体（要约→确认→分片→校验）：send_file 与
    /// send_image_group 共用。msg_id 是进度/状态事件的回指 ID（单文件
    /// 时是文件消息 ID，图片组时是成员 ID）；group 为图片组归属。
    /// 本函数不碰消息状态——调用方按结果收尾（组消息是多成员聚合）。
    #[allow(clippy::too_many_arguments)]
    async fn run_file_transfer(
        &self,
        session: &Arc<PeerSession>,
        path: &Path,
        name: String,
        size: u64,
        sha: String,
        transfer_id: String,
        msg_id: String,
        group: Option<GroupRef>,
    ) -> Result<TransferRun, ServiceError> {
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
                msg_id: msg_id.clone(),
                name: name.clone(),
                size,
                sha256: sha,
                mime: None,
                group,
            }))
            .await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| ServiceError::Other("等待对方接收确认超时".into()))?
            .map_err(|_| ServiceError::Other("对方接收通道已关闭".into()))?;
        if self.take_cancelled(&transfer_id).await {
            self.finish_cancelled(&transfer_id, &msg_id).await;
            return Ok(TransferRun::Cancelled);
        }
        let missing = missing_ranges(&received, size);
        self.emit(Event::TransferProgress {
            transfer_id: transfer_id.clone(),
            msg_id: msg_id.clone(),
            received: covered_bytes(&received),
            total: size,
        });

        // 按缺失区间流式发送
        let send_started = std::time::Instant::now();
        let mut wire_bytes: u64 = 0; // 实际空中字节（压缩后分片数据之和）
        let mut used_compression = false;
        let mut file = std::fs::File::open(path)?;
        let mut sent_total = covered_bytes(&received);
        for (start, end) in missing {
            let mut offset = start;
            file.seek(SeekFrom::Start(start))?;
            while offset < end {
                if self.take_cancelled(&transfer_id).await {
                    self.finish_cancelled(&transfer_id, &msg_id).await;
                    return Ok(TransferRun::Cancelled);
                }
                let want = ((end - offset) as usize).min(CHUNK_SIZE);
                let mut buf = vec![0u8; want];
                file.read_exact(&mut buf)?;
                let (compression, data) = match try_compress(&buf, &name) {
                    Some(c) => (COMP_ZSTD, c),
                    None => (COMP_NONE, buf),
                };
                wire_bytes += data.len() as u64;
                used_compression |= compression != COMP_NONE;
                session
                    .conn
                    .send(&Frame::Chunk(FileChunk {
                        transfer_id: transfer_id.clone(),
                        offset,
                        compression,
                        data,
                    }))
                    .await?;
                offset += want as u64;
                sent_total += want as u64;
                self.emit(Event::TransferProgress {
                    transfer_id: transfer_id.clone(),
                    msg_id: msg_id.clone(),
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
        // 最后一片已发完：先切「等校验」状态，界面显示「已发送，等待
        // 对方校验…」，别让进度条在 100% 无文案干挂到校验回来。
        self.emit(Event::TransferState {
            transfer_id: transfer_id.clone(),
            state: "verifying".into(),
        });
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
        Ok(TransferRun::Done {
            ok,
            wire_bytes,
            used_compression,
            started: send_started,
        })
    }

    /// 发送图片组：多张图片合并为一条消息（v2.6.0）。先把成员清单
    /// （文件 ID/名/大小/哈希）落成一条 ImageGroup 消息，再逐个成员走
    /// 普通文件传输（FileOffer 带 group 归属，接收端据此收进同一条组
    /// 消息）。成员失败不堵后面：全部成功组消息标 Delivered，任一失败
    /// 标 Failed（与多文件队列「单个失败跳过」口径一致）。返回组消息 ID。
    pub async fn send_image_group(&self, paths: &[PathBuf]) -> Result<String, ServiceError> {
        if paths.len() < 2 {
            return Err(ServiceError::Other("图片组至少需要 2 张图片".into()));
        }
        let session = self.session().await?;
        // 先备齐成员清单（文件记录先行落库，清单引用它们的 ID）
        let mut items: Vec<GroupItem> = Vec::with_capacity(paths.len());
        let mut prepared: Vec<(PathBuf, String, u64, String, String, String)> =
            Vec::with_capacity(paths.len());
        for (i, path) in paths.iter().enumerate() {
            let meta = std::fs::metadata(path)?;
            let size = meta.len();
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .ok_or_else(|| ServiceError::Other("文件名无效".into()))?;
            let sha = sha256_file(path)?;
            let file_id = new_id();
            self.storage.save_file(&FileMeta {
                id: file_id.clone(),
                name: name.clone(),
                size,
                sha256: sha.clone(),
                path: path.to_string_lossy().to_string(),
                mime: None,
            })?;
            let member_msg_id = new_id();
            items.push(GroupItem {
                msg_id: member_msg_id.clone(),
                file_id,
                name: name.clone(),
                size,
                sha256: sha.clone(),
                idx: i as u32,
            });
            // 成员传输 ID 掺入成员消息 ID 派生：同一张图在组里出现两次也不撞
            let transfer_id =
                derive_transfer_id(&format!("{sha}:{member_msg_id}"), &session.peer.device_id);
            prepared.push((path.clone(), name, size, sha, transfer_id, member_msg_id));
        }
        let group_id = new_id();
        let mut msg = Message {
            id: group_id.clone(),
            conversation_id: session.conversation_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::ImageGroup,
            body: group_manifest_json(&items),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sending,
            created_at: now_ms(),
            seq: 0,
        };
        msg.seq = self.storage.insert_message(&msg)?;
        self.emit(Event::MessageNew(msg));
        let total = prepared.len() as u32;
        let mut failed = 0u32;
        for (i, (path, name, size, sha, transfer_id, member_msg_id)) in
            prepared.into_iter().enumerate()
        {
            let run = self
                .run_file_transfer(
                    &session,
                    &path,
                    name,
                    size,
                    sha,
                    transfer_id,
                    member_msg_id,
                    Some(GroupRef {
                        group_id: group_id.clone(),
                        index: i as u32,
                        total,
                    }),
                )
                .await;
            match run {
                Ok(TransferRun::Done { ok: true, .. }) => {}
                Ok(_) => failed += 1,
                Err(e) => {
                    failed += 1;
                    log::log_line(
                        "files",
                        &format!("图片组成员发送失败（第 {} 张）：{e}", i + 1),
                    );
                }
            }
        }
        let status = if failed == 0 {
            MessageStatus::Delivered
        } else {
            MessageStatus::Failed
        };
        self.storage.update_message_status(&group_id, status)?;
        self.emit(Event::MessageStatus {
            id: group_id.clone(),
            status,
        });
        log::log_line(
            "files",
            &format!("图片组发送结束：共 {total} 张，失败 {failed} 张"),
        );
        Ok(group_id)
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
        group: Option<GroupRef>,
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
        // 增量哈希只对全新传输启用：已有续传区间时前缀内容不在内存里，
        // FileDone 时回退整文件重算（on_file_done 处理）。
        let fresh = ranges.is_empty();
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
                hasher: if fresh { Some(Sha256::new()) } else { None },
                hashed_upto: 0,
            },
        );
        self.storage.save_file(&FileMeta {
            id: file_id.clone(),
            name: name.clone(),
            size,
            sha256: sha256.clone(),
            path: String::new(),
            mime: None,
        })?;
        match &group {
            // 图片组成员：不单独建消息行，并入组消息清单（首个成员建组）
            Some(g) => {
                self.upsert_group_member(
                    session,
                    g,
                    GroupItem {
                        msg_id: msg_id.clone(),
                        file_id: file_id.clone(),
                        name: name.clone(),
                        size,
                        sha256: sha256.clone(),
                        idx: g.index,
                    },
                )?;
            }
            None => {
                let mut m = Message {
                    id: msg_id,
                    conversation_id: session.conversation_id.clone(),
                    sender: Sender::Peer,
                    kind: MessageKind::File,
                    body: name,
                    file_id: Some(file_id),
                    reply_to: None,
                    status: MessageStatus::Received,
                    created_at: now_ms(),
                    seq: 0,
                };
                m.seq = self.storage.insert_message(&m)?;
                self.emit(Event::MessageNew(m));
            }
        }
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

    /// 图片组成员到达：把成员并入组消息清单。首个成员创建组消息；
    /// 后续成员只更新 body（rowid/seq 不动，组气泡不跳位），清单按
    /// 成员 idx 排序还原发送顺序。同一成员重复要约（断线重发）时
    /// 刷新其 file_id，不重复追加。
    fn upsert_group_member(
        &self,
        session: &Arc<PeerSession>,
        g: &GroupRef,
        item: GroupItem,
    ) -> Result<(), ServiceError> {
        match self.storage.get_message(&g.group_id)? {
            Some(mut gm) => {
                let mut items = parse_group_manifest(&gm.body);
                if let Some(ex) = items.iter_mut().find(|it| it.msg_id == item.msg_id) {
                    ex.file_id = item.file_id.clone();
                } else {
                    items.push(item);
                    items.sort_by_key(|it| it.idx);
                }
                let body = group_manifest_json(&items);
                self.storage.update_message_body(&g.group_id, &body)?;
                gm.body = body;
                self.emit(Event::MessageUpdated(gm));
            }
            None => {
                let mut m = Message {
                    id: g.group_id.clone(),
                    conversation_id: session.conversation_id.clone(),
                    sender: Sender::Peer,
                    kind: MessageKind::ImageGroup,
                    body: group_manifest_json(&[item]),
                    file_id: None,
                    reply_to: None,
                    status: MessageStatus::Received,
                    created_at: now_ms(),
                    seq: 0,
                };
                m.seq = self.storage.insert_message(&m)?;
                self.emit(Event::MessageNew(m));
            }
        }
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
        let data = if chunk.compression == COMP_NONE {
            chunk.data
        } else {
            decompress(&chunk.data, chunk.compression)?
        };
        let mut f = std::fs::OpenOptions::new()
            .read(true)
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
        // 增量哈希：本片若正好接上已哈希前缀，直接喂内存数据；随后把
        // 前缀之后已落盘的连续区间（乱序先到的片）从磁盘补喂，直到
        // 遇到空缺。任何异常都置 None 回退整文件重算，不影响校验。
        if st.hasher.is_some() {
            let data_len = data.len() as u64;
            if chunk.offset <= st.hashed_upto && chunk.offset + data_len > st.hashed_upto {
                let skip = (st.hashed_upto - chunk.offset) as usize;
                if let Some(h) = st.hasher.as_mut() {
                    h.update(&data[skip..]);
                }
                st.hashed_upto += data_len - skip as u64;
            }
            loop {
                let next_end = st
                    .ranges
                    .iter()
                    .find(|(s, e)| *s <= st.hashed_upto && *e > st.hashed_upto)
                    .map(|(_, e)| *e);
                let Some(end) = next_end else { break };
                let span = (end - st.hashed_upto) as usize;
                let mut buf = vec![0u8; span];
                let read_ok = f
                    .seek(SeekFrom::Start(st.hashed_upto))
                    .and_then(|_| f.read_exact(&mut buf))
                    .is_ok();
                if !read_ok {
                    st.hasher = None;
                    break;
                }
                if let Some(h) = st.hasher.as_mut() {
                    h.update(&buf);
                }
                st.hashed_upto = end;
            }
        }
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
        // 先给本方界面一个「校验中」状态，别让进度条在 100% 干挂着
        self.emit(Event::TransferState {
            transfer_id: transfer_id.clone(),
            state: "verifying".into(),
        });
        let mut st = self
            .recv_files
            .lock()
            .await
            .remove(&transfer_id)
            .ok_or_else(|| ServiceError::Other("完成信号对应未知传输".into()))?;
        // 增量哈希覆盖全文件时直接比对摘要，免去整文件重读（大文件
        // 这一步原来要数秒）；否则（续传/乱序补片/中途异常）回退现行
        // 整文件重算，校验强度不降。
        let (actual, verify_how) = if st.hasher.is_some() && st.hashed_upto == st.total {
            let digest = format!("{:x}", st.hasher.take().unwrap().finalize());
            (digest, "增量哈希")
        } else {
            // 整文件重算挪出接收循环：大文件要读数秒，堵在循环里会让
            // 心跳与其它帧都读不到（v2.5.2 实测撞出：校验期会话被自家
            // 心跳误判死亡）。spawn_blocking 里算，循环保持通畅。
            let part = st.part_path.clone();
            let digest = tokio::task::spawn_blocking(move || sha256_file(&part))
                .await
                .map_err(|e| ServiceError::Other(format!("校验任务失败：{e}")))??;
            (digest, "整文件重算")
        };
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
                    "接收完成：{}（{} 字节，校验：{}）→ {}",
                    st.name,
                    st.total,
                    verify_how,
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

    /// 图片组的全部成员文件信息（组气泡缩略图/图片查看器/保存全部
    /// 用）：按清单顺序逐个定位本地路径，定位口径与 file_info 一致
    /// （记录路径优先，缺失时按下载目录 + 文件名兜底）。
    pub fn group_files(&self, message_id: &str) -> Result<Vec<MessageFileInfo>, ServiceError> {
        let Some(m) = self.storage.get_message(message_id)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for it in parse_group_manifest(&m.body) {
            let Some(meta) = self.storage.get_file(&it.file_id)? else {
                continue;
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
            out.push(MessageFileInfo {
                name: meta.name,
                size: meta.size,
                path: path.map(|p| p.to_string_lossy().to_string()),
                exists,
            });
        }
        Ok(out)
    }

    /// 「保存全部图片」：把组内已落盘的图片按发送顺序复制到目标
    /// 目录，命名为 1、2、3…（保留各自扩展名，序号与组内位置对齐，
    /// 缺失的成员跳过不占号错位）。返回（成功数， 总数）。
    pub fn save_group_images(
        &self,
        message_id: &str,
        dir: &Path,
    ) -> Result<(usize, usize), ServiceError> {
        let files = self.group_files(message_id)?;
        std::fs::create_dir_all(dir)?;
        let mut ok = 0usize;
        for (i, f) in files.iter().enumerate() {
            if !f.exists {
                continue;
            }
            let Some(src) = &f.path else { continue };
            let ext = Path::new(&f.name)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let dst = dir.join(format!("{}{}", i + 1, ext));
            if std::fs::copy(src, &dst).is_ok() {
                ok += 1;
            }
        }
        Ok((ok, files.len()))
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
        let mut q = Message {
            id: new_id(),
            conversation_id: conv_id.to_string(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: now_ms(),
            seq: 0,
        };
        let qid = q.id.clone();
        q.seq = self.storage.insert_message(&q)?;
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
        let mut a = Message {
            id: new_id(),
            conversation_id: conv_id.to_string(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: Some(qid),
            status: MessageStatus::Received,
            created_at: now_ms(),
            seq: 0,
        };
        a.seq = self.storage.insert_message(&a)?;
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
        let mut user_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Me,
            kind: MessageKind::Text,
            body: text.to_string(),
            file_id: None,
            reply_to: None,
            status: MessageStatus::Sent,
            created_at: now_ms(),
            seq: 0,
        };
        user_msg.seq = self.storage.insert_message(&user_msg)?;
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

        let mut ai_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
            seq: 0,
        };
        ai_msg.seq = self.storage.insert_message(&ai_msg)?;
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
        let mut ai_msg = Message {
            id: new_id(),
            conversation_id: conv_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: full,
            file_id: None,
            reply_to: None,
            status: MessageStatus::Received,
            created_at: now_ms(),
            seq: 0,
        };
        ai_msg.seq = self.storage.insert_message(&ai_msg)?;
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
                // 空分片不入等待通道：空 Ok 是 Done 的结束哨兵，混入会误判结束。
                if delta.is_empty() {
                    return;
                }
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
                // 空增量不转发：上游流式首片常只有角色、内容为空，而请求端把
                // 「空增量」保留作 Done 的结束哨兵，转发空片会让对方误判结束。
                if d.is_empty() {
                    continue;
                }
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

#[cfg(test)]
mod compression_tests {
    use super::*;

    #[test]
    fn zstd分片压缩往返一致() {
        let data = "可压缩的文本内容，用于验证 zstd 往返。"
            .repeat(200)
            .into_bytes();
        let packed = try_compress(&data, "记录.txt").expect("文本应可压缩");
        assert!(packed.len() < data.len());
        let back = decompress(&packed, COMP_ZSTD).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn 已知压缩格式跳过压缩() {
        let data = vec![7u8; 4096];
        assert!(try_compress(&data, "归档.zip").is_none());
        assert!(try_compress(&data, "照片.JPG").is_none());
        assert!(try_compress(&data, "文档.pdf").is_none());
    }

    #[test]
    fn 压完变大则直传() {
        // 随机数据压不动 → 返回 None 走直传
        let mut x: u32 = 0x12345678;
        let data: Vec<u8> = (0..2048)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 24) as u8
            })
            .collect();
        assert!(try_compress(&data, "随机.bin").is_none());
    }

    #[test]
    fn 旧版zlib标志干净失败并提示升级() {
        let err = decompress(b"whatever", COMP_ZLIB_LEGACY).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("旧版") && msg.contains("升级"),
            "提示原文：{msg}"
        );
    }

    #[test]
    fn 未知压缩标志干净失败() {
        let err = decompress(b"whatever", 9).unwrap_err();
        assert!(format!("{err}").contains("不认识的压缩算法"));
    }
}

#[cfg(test)]
mod group_save_tests {
    use super::*;

    // 「保存全部图片」：按组内顺序存为 1、2、3…（保留原扩展名）；
    // 成员文件缺失时跳过且不补号错位（第 2 张缺失 → 只有 1.jpg 与 3.jpg）。
    #[test]
    fn 保存全部图片按序命名且缺失不补号() {
        let base = std::env::temp_dir().join(format!("bl-group-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = Node::new(&base, "测试机").expect("建节点");
        let dl = node.download_dir();
        std::fs::create_dir_all(&dl).unwrap();
        let mut items = Vec::new();
        for (i, name) in ["a.jpg", "b.png", "c.jpg"].iter().enumerate() {
            let fid = format!("file-{i}");
            let p = dl.join(name);
            if i != 1 {
                std::fs::write(&p, format!("data-{i}")).unwrap();
            }
            node.storage
                .save_file(&FileMeta {
                    id: fid.clone(),
                    name: name.to_string(),
                    size: 6,
                    sha256: String::new(),
                    path: p.to_string_lossy().to_string(),
                    mime: None,
                })
                .unwrap();
            items.push(GroupItem {
                msg_id: format!("m-{i}"),
                file_id: fid,
                name: name.to_string(),
                size: 6,
                sha256: String::new(),
                idx: i as u32,
            });
        }
        node.storage
            .create_conversation(&Conversation {
                id: "conv-1".to_string(),
                kind: ConversationKind::Peer,
                peer_device_id: None,
                title: "对端".to_string(),
                draft: String::new(),
                last_read_id: None,
                created_at: 0,
            })
            .unwrap();
        node.storage
            .insert_message(&Message {
                id: "g1".to_string(),
                conversation_id: "conv-1".to_string(),
                sender: Sender::Peer,
                kind: MessageKind::ImageGroup,
                body: group_manifest_json(&items),
                file_id: None,
                reply_to: None,
                status: MessageStatus::Received,
                created_at: 0,
                seq: 0,
            })
            .unwrap();
        let out = base.join("out");
        let (ok, total) = node.save_group_images("g1", &out).expect("保存");
        assert_eq!((ok, total), (2, 3));
        assert!(out.join("1.jpg").exists());
        assert!(!out.join("2.png").exists(), "缺失成员不落盘也不补号");
        assert!(out.join("3.jpg").exists());
        assert_eq!(
            std::fs::read_to_string(out.join("3.jpg")).unwrap(),
            "data-2",
            "序号与内容对应组内顺序"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
