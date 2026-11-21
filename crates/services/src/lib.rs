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
        /// ㉛ 落点规则的数据面：连上的对端设备 id 与其会话 id，前端据此
        /// 在连接成功后打开「实际连上的那个对端」的会话。
        peer_device_id: Option<String>,
        conversation_id: Option<String>,
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
    /// 思考模式开关没生效（v2.6.1）：请求带了开关，但实际执行一侧的上游
    /// 接口不支持 thinking 参数——前端在回答区给一次性小字提示，不许假装开了。
    AiThinkingNote,
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

/// 用系统默认浏览器打开网址（设置页「项目」地址点击用）。
/// 只放行 http/https，防前端误传本地路径或协议被当成文件打开。
pub fn open_url(url: &str) -> Result<(), ServiceError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ServiceError::Other("只允许打开 http/https 网址".into()));
    }
    open_file(std::path::Path::new(url))
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

/// 一次剪贴板读取的结果：内容（可能为空）+ 是否因「被其他程序占用」读不到。
/// 「剪贴板里没东西」与「被占用读不了」必须分开——前者是常态、后者是
/// 故障信号（v2.6.1 前两者都静默成 None，出问题无据可查）。
struct ClipRead {
    content: Option<ClipContent>,
    occupied: bool,
}

/// 读系统剪贴板：与粘贴规则同口径——先文字，纯图片才取图片；空剪贴板返回 None。
/// 句柄由 Node 长驻持有（见 clip_handle 字段注释），不要在这里新建临时句柄。
fn clip_read_with(cb: &mut arboard::Clipboard) -> ClipRead {
    let mut occupied = false;
    match cb.get_text() {
        Ok(t) => {
            if !t.is_empty() {
                return ClipRead {
                    content: Some(ClipContent::Text(t)),
                    occupied: false,
                };
            }
        }
        Err(arboard::Error::ClipboardOccupied) => occupied = true,
        Err(_) => {}
    }
    match cb.get_image() {
        Ok(img) => {
            return ClipRead {
                content: Some(ClipContent::Image {
                    width: img.width as u32,
                    height: img.height as u32,
                    rgba: img.bytes.into_owned(),
                }),
                occupied: false,
            };
        }
        Err(arboard::Error::ClipboardOccupied) => occupied = true,
        Err(_) => {}
    }
    ClipRead {
        content: None,
        occupied,
    }
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
/// write_backoff_until / last_read_fail_log 是 v2.6.1 的退让与限频字段：
/// 同步绝不许影响本机功能——写不进去立刻退让、不许重试风暴；读失败
/// 要留痕但不许刷屏。
#[derive(Default)]
struct ClipGuard {
    last_seen: Option<String>,
    suppress: Option<String>,
    /// 写剪贴板失败后的退让截止时刻：此前到达的远端内容直接丢弃不再尝试写。
    write_backoff_until: Option<std::time::Instant>,
    /// 上次记录「读剪贴板被占用」的时刻（限频：每分钟至多一条）。
    last_read_fail_log: Option<std::time::Instant>,
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
    /// ⑳ 分段计时：首片到达时刻、已收片数、收片处理累计耗时（毫秒）。
    first_chunk_at: Option<std::time::Instant>,
    chunk_count: u64,
    process_ms: f64,
    /// ⑳ 续传进度落盘节流计数：距上次 save_transfer 的片数。
    since_save: u32,
    /// ⑳ .part 文件常驻句柄（首片时打开）：免逐片 open/close。
    file: Option<std::fs::File>,
}

/// 对端代理应答通道的消息（v2.6.1 起）：增量与完成分开建模，
/// 完成时除错误外还带「思考开关是否真被上游接受」，请求方据此如实提示。
enum ProxyRecv {
    Delta(String),
    Done {
        error: Option<String>,
        thinking_applied: Option<bool>,
    },
}

/// 一次 AI 调用的结果：正文 + 思考开关的生效情况
/// （None = 没带开关或无从得知，如对端是旧版本；Some(false) = 带了但没生效）。
pub struct AiOutcome {
    pub text: String,
    pub thinking_applied: Option<bool>,
}

/// AI 请求级选项的服务层别名（与 ai 层同构，直接透传）。
pub type AiCallOptions = bluelink_ai::AiRequestOptions;

/// ㉓ 历史上下文 token 预算（估算口径，见 estimate_tokens）：组装上下文
/// 时从最新往旧装、装满为止，被丢弃的老内容改走摘要纪要。
pub const AI_HISTORY_TOKEN_BUDGET: usize = 24_000;

/// ㉓ 历史摘要的长度上限（字）。
pub const AI_SUMMARY_MAX_CHARS: usize = 300;

/// 粗估 token 数（启发式估算、非精确分词，只用于预算裁剪不作计费
/// 依据）：非 ASCII 字符（中文等）约 1 字 1 token，ASCII 约 4 字符
/// 1 token。
pub fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut ascii_run = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_run += 1;
        } else {
            tokens += (ascii_run + 3) / 4 + 1;
            ascii_run = 0;
        }
    }
    tokens + (ascii_run + 3) / 4
}

/// ㉓ 预算裁剪（纯函数，组装与测试共用）：bodies 为正序历史正文，返回
/// 保留段的起始下标——从最新往旧累加估算 token（每条 +4 结构开销），
/// 装满预算为止；最新一条无条件保留（最新一轮不许截断）。
pub fn ai_kept_from(bodies: &[&str]) -> usize {
    let mut used = 0usize;
    let mut kept = 0usize;
    for body in bodies.iter().rev() {
        let cost = estimate_tokens(body) + 4;
        if kept > 0 && used + cost > AI_HISTORY_TOKEN_BUDGET {
            break;
        }
        used += cost;
        kept += 1;
    }
    bodies.len() - kept
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
    proxy_waiters: Mutex<HashMap<String, tokio::sync::mpsc::Sender<ProxyRecv>>>,
    /// 双边同连仲裁（v2.6.1，见 begin_outbound_attempt）：出站尝试登记的
    /// （目标设备名， 代号）；握手期间同名设备入站时按设备 ID 字典序裁决。
    outbound_mark: Mutex<Option<(String, u64)>>,
    /// 被裁决让行的出站尝试代号（宿主侧握手写入、发起侧握手以
    /// 100ms 轮询检查——握手最长 20 秒，轮询无丢唤醒竞态、代价可忽略）。
    glare_yield_gen: Mutex<Option<u64>>,
    outbound_gen: std::sync::atomic::AtomicU64,
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
            outbound_mark: Mutex::new(None),
            glare_yield_gen: Mutex::new(None),
            outbound_gen: std::sync::atomic::AtomicU64::new(0),
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

    /// 登记一次出站连接尝试（双边同连仲裁用）：bt_connect 在传输连接前
    /// 调用，返回本次尝试的代号；attach 结束时以 end_outbound_attempt 注销。
    /// 只有经 bt_connect 的蓝牙连接登记——局域网通道不登记、不参与仲裁。
    pub async fn begin_outbound_attempt(&self, target_name: &str) -> u64 {
        let gen = self
            .outbound_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        *self.outbound_mark.lock().await = Some((target_name.to_string(), gen));
        gen
    }

    /// 注销出站尝试登记（代号对得上才清，防迟到的旧尝试清掉新尝试的登记）。
    pub async fn end_outbound_attempt(&self, gen: u64) {
        let mut mark = self.outbound_mark.lock().await;
        if matches!(&*mark, Some((_, g)) if *g == gen) {
            *mark = None;
        }
    }

    /// 等待「本代号已被裁决让行」：100ms 轮询检查裁决槽（见 glare_yield_gen 注释）。
    async fn wait_glare_yield(&self, gen: u64) {
        loop {
            if *self.glare_yield_gen.lock().await == Some(gen) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
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
                    // 双边同连仲裁（v2.6.1）：本机正向同一台设备发起出站
                    // 握手时，对方的出站也连了进来——两条链路只能活一条。
                    // 规则双端一致、结果确定：设备 ID 字典序大的一方让行
                    // （取消自己的出站、接受入站），小的一方保留出站、
                    // 拒绝入站。匹配键是设备名（扫描名与 Hello 名同源）。
                    {
                        let mark = self.outbound_mark.lock().await.clone();
                        if let Some((target_name, gen)) = mark {
                            if target_name == device_name {
                                if self.device_id > device_id {
                                    *self.glare_yield_gen.lock().await = Some(gen);
                                    log::log_line(
                                        "conn",
                                        "双边同连仲裁：对方同时也在连本机，本机设备 ID 较大，让行出站尝试、改走对方连入的链路",
                                    );
                                } else {
                                    log::log_line(
                                        "conn",
                                        "双边同连仲裁：对方同时也在连本机，本机设备 ID 较小，保留出站尝试、拒绝这条入站连接",
                                    );
                                    return Err(ServiceError::Other(
                                        "双边同连仲裁：已拒绝入站（本机出站优先）".into(),
                                    ));
                                }
                            }
                        }
                    }
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
            // 双边同连仲裁：登记过出站尝试时，让行裁决与等 Ack 赛跑——
            // 对方（大 ID 方让行后）连入的链路才是要活的那条，本尝试立刻收尾。
            let my_gen = self
                .outbound_mark
                .lock()
                .await
                .as_ref()
                .map(|(_, g)| *g);
            let first = match my_gen {
                Some(gen) => tokio::select! {
                    biased;
                    _ = self.wait_glare_yield(gen) => {
                        log::log_line(
                            "conn",
                            "双边同连仲裁：本机出站尝试让行，改走对方连入的链路",
                        );
                        return Err(ServiceError::Other("双边同连：已让行入站连接".into()));
                    }
                    r = conn.recv() => r?,
                },
                None => conn.recv().await?,
            };
            match first {
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
            peer_device_id: Some(session.peer.device_id.clone()),
            conversation_id: Some(session.conversation_id.clone()),
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
                    peer_device_id: None,
                    conversation_id: None,
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
            peer_device_id: None,
            conversation_id: None,
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
                thinking,
                temperature,
                max_tokens,
                top_p,
                ..
            }) => {
                let node = self.clone();
                let session2 = session.clone();
                let opts = AiCallOptions {
                    thinking,
                    temperature,
                    max_tokens,
                    top_p,
                };
                tokio::spawn(async move {
                    let _ = node
                        .serve_ai_proxy(session2, request_id, messages, &opts)
                        .await;
                });
            }
            Frame::Control(Control::AiProxyChunk { request_id, delta }) => {
                self.handle_ai_proxy_frame(Control::AiProxyChunk { request_id, delta })
                    .await;
            }
            Frame::Control(Control::AiProxyDone {
                request_id,
                error,
                thinking_applied,
            }) => {
                self.handle_ai_proxy_frame(Control::AiProxyDone {
                    request_id,
                    error,
                    thinking_applied,
                })
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

    /// 把图片文件复制进系统剪贴板（图片查看器「复制图片」，⑱）：
    /// 按路径读文件、解码为 RGBA、经 arboard 原生通道写入——不走
    /// WebView 的浏览器剪贴板 API（写图不可靠）。走与剪贴板同步同一把
    /// 长驻句柄（with_clipboard），不另开实例抢占。同步阻塞调用，
    /// 调用方须放在 spawn_blocking 里。返回图片尺寸供界面反馈。
    /// 失败口径全是人话：文件不在了/解码失败/剪贴板被占用或不可用。
    pub fn copy_image_blocking(&self, path: &std::path::Path) -> Result<(u32, u32), ServiceError> {
        if !path.is_file() {
            return Err(ServiceError::Other(
                "图片文件已不存在，无法复制".into(),
            ));
        }
        let img = image::open(path).map_err(|_| {
            ServiceError::Other("这张图片解码失败，无法复制".into())
        })?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        // 尺寸上限保护：超大图 RGBA 展开是 4 字节/像素，5000 万像素
        // 已是 200MB 量级，再大直接拒绝并说明，不把应用拖垮。
        if (w as u64) * (h as u64) > 50_000_000 {
            return Err(ServiceError::Other(
                "图片尺寸过大，无法复制到剪贴板".into(),
            ));
        }
        let data = arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        };
        match self.with_clipboard(|cb| cb.set_image(data)) {
            Some(Ok(())) => Ok((w, h)),
            Some(Err(_)) => Err(ServiceError::Other(
                "剪贴板正被其他程序占用，请稍后再试".into(),
            )),
            None => Err(ServiceError::Other("系统剪贴板不可用".into())),
        }
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
    ///
    /// 基线规矩（v2.6.1，用户裁定「同步不得影响任何电脑本身的功能」）：
    /// 每次（重新）进入「双方都开」状态时，先把本机当前剪贴板登记为基线
    /// （只记哈希、绝不发送），之后只同步基线之后新复制的内容。旧行为在
    /// 每次重连时把连接前就躺在剪贴板里的旧内容无条件推给对方、写进对方
    /// 剪贴板，会顶掉对方刚复制的东西（真机实证：同一段文字随重连被重写
    /// 5 次）——基线制从根上消除。
    fn spawn_clipboard_poll(self: &Arc<Self>, session: Arc<PeerSession>) {
        let node = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(700));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut primed = false;
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
                    // 任一侧关掉后，下次再进入双开状态要重新立基线
                    primed = false;
                    continue;
                }
                if !primed {
                    node.clipboard_prime().await;
                    primed = true;
                    continue;
                }
                node.clipboard_poll_once(&session).await;
            }
        });
    }

    /// 立基线：读一次本机剪贴板，把内容哈希登记为已见（不发送、不写入）。
    async fn clipboard_prime(self: &Arc<Self>) {
        let node = self.clone();
        let read = tokio::task::spawn_blocking(move || node.with_clipboard(clip_read_with))
            .await
            .ok()
            .flatten();
        if let Some(ClipRead {
            content: Some(content),
            ..
        }) = read
        {
            let mut g = self.clip_guard.lock().await;
            g.last_seen = Some(content.hash());
        }
        log::log_verbose(
            "clipboard",
            "剪贴板同步基线已建立：只同步此后新复制的内容，重连不重推旧内容",
        );
    }

    /// 一次监视：读本机剪贴板，内容变了且不是刚从对方写入的才发送。
    async fn clipboard_poll_once(self: &Arc<Self>, session: &Arc<PeerSession>) {
        // 剪贴板读取是同步系统调用，放阻塞线程执行，避免卡住异步运行时
        let node = self.clone();
        let read = tokio::task::spawn_blocking(move || node.with_clipboard(clip_read_with))
            .await
            .ok()
            .flatten();
        let Some(read) = read else { return };
        if read.occupied && read.content.is_none() {
            // 被其他程序占用读不到：留痕但限频（每分钟至多一条），不重试硬抢
            let mut g = self.clip_guard.lock().await;
            let due = g
                .last_read_fail_log
                .map(|t| t.elapsed() >= std::time::Duration::from_secs(60))
                .unwrap_or(true);
            if due {
                g.last_read_fail_log = Some(std::time::Instant::now());
                drop(g);
                log::log_line(
                    "clipboard",
                    "读取本机剪贴板失败：剪贴板被其他程序占用，本轮跳过（不重试）",
                );
            }
            return;
        }
        let Some(content) = read.content else { return };
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
        {
            let g = self.clip_guard.lock().await;
            // 本机剪贴板已经是这份内容：再写一遍纯属多余，还可能顶掉用户
            // 此刻刚复制的新内容——跳过（同步绝不影响本机功能的第一道闸）
            if g.last_seen.as_deref() == Some(hash.as_str()) {
                log::log_verbose(
                    "clipboard",
                    "收到对方剪贴板内容，与本机当前内容相同，跳过写入",
                );
                return;
            }
            // 上次写入失败后的退让期内：直接丢弃，绝不重试硬抢系统剪贴板
            if g.write_backoff_until
                .map(|t| t > std::time::Instant::now())
                .unwrap_or(false)
            {
                log::log_verbose(
                    "clipboard",
                    "剪贴板写入退让中，丢弃本条远端内容（不重试）",
                );
                return;
            }
        }
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
                self.clip_write_failed(&e).await;
            }
            Err(e) => {
                self.clip_write_failed(&e.to_string()).await;
            }
        }
    }

    /// 写本机剪贴板失败的统一收尾：记日志 + 进入 30 秒退让期。
    /// 失败多半是剪贴板被其他程序占用——同步是旁观者，退让不重试，
    /// 绝不许因为同步把用户本机的复制粘贴搞坏（用户 2026-11-21 裁定）。
    async fn clip_write_failed(&self, err: &str) {
        {
            let mut g = self.clip_guard.lock().await;
            g.write_backoff_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        }
        log::log_line(
            "clipboard",
            &format!("写入本机剪贴板失败：{err}（30 秒内不再尝试写入，不重试）"),
        );
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
        let verify_wait_t0 = std::time::Instant::now(); // ⑳ 发送端等校验计时
        let ok = tokio::time::timeout(std::time::Duration::from_secs(60), vrx)
            .await
            .map_err(|_| ServiceError::Other("等待校验结果超时".into()))?
            .map_err(|_| ServiceError::Other("校验通道已关闭".into()))?;
        log_line(
            "files",
            &format!(
                "essential: 发送计时 {}（{} 字节）：最后一片发出→收到校验确认 {:.0} ms（发片总耗时 {:.0} ms）",
                name,
                size,
                verify_wait_t0.elapsed().as_secs_f64() * 1000.0,
                send_started.elapsed().as_secs_f64() * 1000.0,
            ),
        );
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
                first_chunk_at: None,
                chunk_count: 0,
                process_ms: 0.0,
                since_save: 0,
                file: None,
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
        let t0 = std::time::Instant::now(); // ⑳ 收片处理计时
        let mut guard = self.recv_files.lock().await;
        let st = guard
            .get_mut(&chunk.transfer_id)
            .ok_or_else(|| ServiceError::Other("收到未知传输的分片".into()))?;
        if st.first_chunk_at.is_none() {
            st.first_chunk_at = Some(t0);
        }
        st.chunk_count += 1;
        let data = if chunk.compression == COMP_NONE {
            chunk.data
        } else {
            decompress(&chunk.data, chunk.compression)?
        };
        // ⑳ .part 句柄常驻（首片打开、整程复用）：逐片 open/close 在
        // 快速链路上是纯浪费，还拖慢接收队列排空、让对方等校验更久。
        if st.file.is_none() {
            st.file = Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&st.part_path)?,
            );
        }
        let f = st.file.as_mut().unwrap();
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
        st.process_ms += t0.elapsed().as_secs_f64() * 1000.0;
        // ⑳ 续传进度落盘节流：逐片一次 INSERT（各带一次落盘同步）是
        // 收片处理的大头，改成每 32 片落一次；崩溃时最多丢最近 32 片
        // （8 MB）的续传进度、重连后补发，换取整程排空速度。
        st.since_save += 1;
        let save_due = st.since_save >= 32;
        if save_due {
            st.since_save = 0;
        }
        let (tid, written, total, ranges, mid) = (
            chunk.transfer_id.clone(),
            st.written,
            st.total,
            st.ranges.clone(),
            st.msg_id.clone(),
        );
        drop(guard);
        if save_due {
            self.storage
                .save_transfer(&tid, "", "in", "", total, &ranges, "receiving")?;
        }
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
        let done_t0 = std::time::Instant::now(); // ⑳ 「收完→发出确认」计时
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
        let verify_t0 = std::time::Instant::now();
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
        let verify_ms = verify_t0.elapsed().as_secs_f64() * 1000.0;
        let ok = actual == st.sha256;
        if ok {
            let final_path = self.download_dir().join(&st.name);
            std::fs::rename(&st.part_path, &final_path)?;
            // ⑳ 三笔回写（消息状态/落地路径/传输收尾）合并成一个事务，
            // 只剩一次提交落盘——文件菜单靠落地路径定位文件，别漏。
            self.storage.finish_received_file(
                &st.msg_id,
                &st.file_id,
                &final_path.to_string_lossy(),
                &transfer_id,
                &session.peer.device_id,
                st.total,
                &st.ranges,
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
        // ⑳ 分段计时落日志：收片跨度（首片→FileDone 被处理）、收片处理
        // 累计、校验耗时、收完→确认总耗时。发送端另有一条等校验日志，
        // 两边相减即帧在接收队列里排队被嚼的时间。
        let span_ms = st
            .first_chunk_at
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        log_line(
            "files",
            &format!(
                "essential: 接收计时 {}（{} 字节，{} 片）：收片跨度 {:.0} ms，收片处理累计 {:.0} ms，校验（{}）{:.0} ms，收完→发出确认 {:.0} ms",
                st.name,
                st.total,
                st.chunk_count,
                span_ms,
                st.process_ms,
                verify_how,
                verify_ms,
                done_t0.elapsed().as_secs_f64() * 1000.0,
            ),
        );
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

    /// ㉕ 新建一条 AI 会话：id 以 ai- 开头（定位链路按前缀识别 AI 会话），
    /// 标题默认「新会话」，用户可改名。ai-default「AI 助手」不受影响。
    pub fn create_ai_conversation(
        &self,
        title: Option<String>,
    ) -> Result<Conversation, ServiceError> {
        let conv = Conversation {
            id: format!("ai-{}", new_id()),
            kind: ConversationKind::Ai,
            peer_device_id: None,
            title: title
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "新会话".into()),
            draft: String::new(),
            last_read_id: None,
            created_at: now_ms(),
        };
        self.storage.create_conversation(&conv)?;
        Ok(conv)
    }

    /// ㉕ AI 会话改名并持久化：只许改 Ai 类会话（设备会话名是对方设备名，
    /// 不归用户改）。
    pub fn rename_ai_conversation(
        &self,
        conv_id: &str,
        title: &str,
    ) -> Result<(), ServiceError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ServiceError::Other("会话名不能为空".into()));
        }
        let conv = self
            .storage
            .list_conversations()?
            .into_iter()
            .find(|c| c.id == conv_id)
            .ok_or_else(|| ServiceError::Other("会话不存在".into()))?;
        if conv.kind != ConversationKind::Ai {
            return Err(ServiceError::Other("只能给 AI 会话改名".into()));
        }
        self.storage.rename_conversation(conv_id, title)?;
        Ok(())
    }

    /// ㉕ 解析 AI 页当前打开的会话：前端传了就校验必须是已存在的 Ai
    /// 会话，没传（或问AI 等固定落点）用 ai-default「AI 助手」。
    fn resolve_ai_conversation(
        &self,
        conv_id: Option<String>,
    ) -> Result<String, ServiceError> {
        match conv_id {
            None => self.ensure_ai_conversation(),
            Some(id) => {
                let conv = self
                    .storage
                    .list_conversations()?
                    .into_iter()
                    .find(|c| c.id == id);
                match conv {
                    Some(c) if c.kind == ConversationKind::Ai => Ok(id),
                    _ => Err(ServiceError::Other("AI 会话不存在".into())),
                }
            }
        }
    }

    /// 在 AI 会话里提问：走「默认 AI」来源（本机配置 / 连接设备的 AI / 自动）。
    /// ㉕ 起跟随 AI 页当前打开的会话（conversation_id），不传则落 ai-default。
    pub async fn ai_send(
        self: &Arc<Self>,
        text: &str,
        thinking: Option<bool>,
        conversation_id: Option<String>,
    ) -> Result<Message, ServiceError> {
        let conv_id = self.resolve_ai_conversation(conversation_id)?;
        self.ai_push_and_answer(&conv_id, text, thinking).await
    }

    /// 聊天页「问AI」：把某条聊天消息作为上下文发给默认 AI，问答落在 AI 会话里
    /// （用户消息带出处标题，方便在 AI 页接着追问）。
    pub async fn ai_ask_message(
        self: &Arc<Self>,
        conv_id: &str,
        msg_id: &str,
        thinking: Option<bool>,
        extra: Option<String>,
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
        // ㉒：引用消息只作上下文；用户补了自己的问题时一并组装，
        // 没补（extra 为空）则按引用原文问（旧行为兜底）。
        let extra = extra.unwrap_or_default();
        let extra = extra.trim();
        let body = if extra.is_empty() {
            format!("【来自与「{title}」的聊天】\n{}", orig.body)
        } else {
            format!(
                "【来自与「{title}」的聊天】\n{}\n\n【我的问题】\n{}",
                orig.body, extra
            )
        };
        log_line(
            "ai",
            &format!(
                "essential: 问AI 发起（引用消息 {}，带补充问题={}）",
                msg_id,
                !extra.is_empty()
            ),
        );
        self.ai_push_and_answer(&ai_conv, &body, thinking).await
    }

    /// 聊天框 @AI 提问：问题与回答都落在当前聊天会话里（仅本机保存，不发给对方），
    /// 来源由前端 @ 选择决定（本机 / 对方设备 / 默认）。
    pub async fn chat_ask_ai(
        self: &Arc<Self>,
        conv_id: &str,
        source: AiSource,
        text: &str,
        thinking: Option<bool>,
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
        let opts = AiCallOptions {
            thinking,
            ..Default::default()
        };
        let outcome = self.chat_core_src(&msgs, None, source, &opts).await?;
        if outcome.thinking_applied == Some(false) {
            self.emit(Event::AiThinkingNote);
        }
        let mut a = Message {
            id: new_id(),
            conversation_id: conv_id.to_string(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: outcome.text,
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

    /// 聊天里 @AI 回答的重新生成（v2.6.1 补齐 ⑭）：按回答消息定位它
    /// 引用的问题，删掉旧回答、用默认来源重新提问，新回答挂回原问题。
    /// 动作记 essential 日志——「点了没反应」必须能凭日志分辨是没发出还是在等。
    pub async fn chat_regenerate_ai(
        self: &Arc<Self>,
        answer_msg_id: &str,
        thinking: Option<bool>,
    ) -> Result<Message, ServiceError> {
        let answer = self
            .storage
            .get_message(answer_msg_id)?
            .ok_or_else(|| ServiceError::Other("要重新生成的回答不存在".into()))?;
        if answer.sender != Sender::Ai {
            return Err(ServiceError::Other("只能重新生成 AI 的回答".into()));
        }
        let qid = answer
            .reply_to
            .clone()
            .ok_or_else(|| ServiceError::Other("这条回答没有对应的问题，无法重新生成".into()))?;
        let question = self
            .storage
            .get_message(&qid)?
            .ok_or_else(|| ServiceError::Other("原问题已不存在，无法重新生成".into()))?;
        log::log_line(
            "ai",
            &format!(
                "重新生成（聊天 @AI 回答）：已发出新请求（会话 {}）",
                answer.conversation_id
            ),
        );
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: question.body.clone(),
        }];
        let opts = AiCallOptions {
            thinking,
            ..Default::default()
        };
        let source = self.default_ai_source();
        // 先生成、新答到手再删旧答：生成失败时原回答不丢
        let outcome = self.chat_core_src(&msgs, None, source, &opts).await?;
        self.storage.delete_message(answer_msg_id)?;
        if outcome.thinking_applied == Some(false) {
            self.emit(Event::AiThinkingNote);
        }
        let mut a = Message {
            id: new_id(),
            conversation_id: answer.conversation_id.clone(),
            sender: Sender::Ai,
            kind: MessageKind::Text,
            body: outcome.text,
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

    /// ㉓ 上下文组装（发送端一处，本机与对端转发两条路共用）：按 token
    /// 预算从最新往旧装历史、装满为止；最新一条无条件完整保留（最新
    /// 一轮不许截断）。被预算丢弃的老内容先摘要成一段纪要（见
    /// ai_summary_for），纪要作为 system 注记放最前面。
    async fn assemble_ai_context(
        self: &Arc<Self>,
        conv_id: &str,
    ) -> Result<Vec<ChatMessage>, ServiceError> {
        let all = self.storage.list_messages(conv_id, None, 100_000)?;
        let bodies: Vec<&str> = all.iter().map(|m| m.body.as_str()).collect();
        let kept_from = ai_kept_from(&bodies);
        let dropped = &all[..kept_from];
        let kept = &all[kept_from..];
        let kept_tokens: usize = kept.iter().map(|m| estimate_tokens(&m.body) + 4).sum();
        log_line(
            "ai",
            &format!(
                "essential: 上下文组装（会话 {conv_id}）：共 {} 条，保留 {} 条（估算 {kept_tokens} token），丢弃 {} 条走摘要",
                all.len(),
                kept.len(),
                dropped.len()
            ),
        );
        let mut out: Vec<ChatMessage> = Vec::new();
        if !dropped.is_empty() {
            if let Some(summary) = self.ai_summary_for(conv_id, dropped).await {
                out.push(ChatMessage {
                    role: "system".into(),
                    content: format!("此前对话摘要：{summary}"),
                });
            }
        }
        for m in kept.iter() {
            out.push(ChatMessage {
                role: if m.sender == Sender::Ai {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: m.body.clone(),
            });
        }
        Ok(out)
    }

    /// ㉓ 被预算丢弃的老内容摘要：纪要按会话存 settings（覆盖水位
    /// ai_summary_upto_<会话> + 正文 ai_summary_text_<会话>）。已有纪要
    /// 只覆盖到水位时，把旧纪要与新丢弃段合并重摘一次；没有新丢弃段
    /// 直接用旧纪要。任何失败都退化（用旧纪要或纯截断），不阻塞发送。
    async fn ai_summary_for(
        self: &Arc<Self>,
        conv_id: &str,
        dropped: &[Message],
    ) -> Option<String> {
        let upto_key = format!("ai_summary_upto_{conv_id}");
        let text_key = format!("ai_summary_text_{conv_id}");
        let old_upto: i64 = self
            .storage
            .get_setting(&upto_key)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let old_text = self
            .storage
            .get_setting(&text_key)
            .ok()
            .flatten()
            .unwrap_or_default();
        let max_seq = dropped.iter().map(|m| m.seq).max().unwrap_or(0);
        if max_seq <= old_upto {
            return if old_text.is_empty() {
                None
            } else {
                Some(old_text)
            };
        }
        let mut src = String::new();
        if !old_text.is_empty() {
            src.push_str(&format!("【已有摘要】{old_text}\n"));
        }
        for m in dropped.iter().filter(|m| m.seq > old_upto) {
            let who = if m.sender == Sender::Ai { "AI" } else { "用户" };
            src.push_str(&format!("{who}：{}\n", m.body));
        }
        let prompt = format!(
            "请把下面这段对话压缩成一段不超过 {} 字的中文纪要，只保留后续对话需要知道的事实、话题与结论，不要寒暄、不要分点罗列，写成连贯的一段话：\n\n{src}",
            AI_SUMMARY_MAX_CHARS
        );
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }];
        let opts = AiCallOptions::default();
        let source = self.default_ai_source();
        // on_delta=None：摘要调用不往界面推流式增量，安静地一次拿全
        match self.chat_core_src(&msgs, None, source, &opts).await {
            Ok(outcome) => {
                let mut text = outcome.text.trim().to_string();
                if text.chars().count() > AI_SUMMARY_MAX_CHARS {
                    text = text.chars().take(AI_SUMMARY_MAX_CHARS).collect();
                }
                if text.is_empty() {
                    return if old_text.is_empty() {
                        None
                    } else {
                        Some(old_text)
                    };
                }
                let _ = self.storage.set_setting(&upto_key, &max_seq.to_string());
                let _ = self.storage.set_setting(&text_key, &text);
                log_line(
                    "ai",
                    &format!(
                        "essential: 历史摘要已更新（会话 {conv_id}，覆盖至 seq {max_seq}，{} 字）",
                        text.chars().count()
                    ),
                );
                Some(text)
            }
            Err(e) => {
                log_line(
                    "ai",
                    &format!("essential: 历史摘要生成失败，退化为纯截断：{e}"),
                );
                if old_text.is_empty() {
                    None
                } else {
                    Some(old_text)
                }
            }
        }
    }

    /// 把一条用户消息写入 AI 会话并生成回复（AI 页提问与「问AI」共用）。
    async fn ai_push_and_answer(
        self: &Arc<Self>,
        conv_id: &str,
        text: &str,
        thinking: Option<bool>,
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

        // ㉓ 组装上下文：token 预算（24K）+ 老内容摘要，不再固定最近 20 条
        let msgs = self.assemble_ai_context(&conv_id).await?;

        let opts = AiCallOptions {
            thinking,
            ..Default::default()
        };
        let full = self.chat_anywhere(&conv_id, &msgs, &opts).await?;

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
    /// 动作记 essential 日志（⑭）：用户点了之后是真发出了请求还是卡在半路，
    /// 凭日志可查，不靠猜。
    pub async fn ai_regenerate(
        self: &Arc<Self>,
        thinking: Option<bool>,
        conversation_id: Option<String>,
    ) -> Result<Message, ServiceError> {
        log::log_line("ai", "重新生成（AI 页）：已发出新请求");
        // ㉕ 跟随 AI 页当前打开的会话重新生成，不传则落 ai-default
        let conv_id = self.resolve_ai_conversation(conversation_id)?;
        let history = self.storage.list_messages(&conv_id, None, 50)?;
        // 找到并删除最后一条 AI 回复
        if let Some(last_ai) = history.iter().rev().find(|m| m.sender == Sender::Ai) {
            self.storage.delete_message(&last_ai.id)?;
        }
        let history = self.storage.list_messages(&conv_id, None, 20)?;
        if !history.iter().any(|m| m.sender == Sender::Me) {
            return Err(ServiceError::Other("还没有提问过，无法重新生成".into()));
        }
        // ㉓ 重新生成同口径：token 预算组装 + 老内容摘要
        let msgs = self.assemble_ai_context(&conv_id).await?;
        let opts = AiCallOptions {
            thinking,
            ..Default::default()
        };
        let full = self.chat_anywhere(&conv_id, &msgs, &opts).await?;
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

    /// 按指定来源提问：Local 只走本机配置、Peer 只走对端转发、Auto 本机优先否则转发。
    /// opts 是请求级选项（思考开关/网关透传参数），随请求走、端到端不落配置。
    /// 路由决定与失败都记 essential 日志（v2.6.1 补可见性：此前 AI 请求在
    /// 普通模式日志里零记录，TRAE 422 一类问题无据可查）。日志只记模型名
    /// 与错误原文（错误文案本身不含密钥），绝不记密钥与提示词全文。
    async fn chat_core_src(
        &self,
        msgs: &[ChatMessage],
        on_delta: Option<&mut (dyn FnMut(&str) + Send)>,
        source: AiSource,
        opts: &AiCallOptions,
    ) -> Result<AiOutcome, ServiceError> {
        let cfg = self.ai_cfg.lock().await.clone();
        if source != AiSource::Peer {
            if let Some(cfg) = cfg.filter(|c| c.is_ready()) {
                log::log_line(
                    "ai",
                    &format!(
                        "AI 请求路由：本机接口（模型 {}，{} 条消息）",
                        cfg.model,
                        msgs.len()
                    ),
                );
                let client = AiClient::new(cfg.clone());
                let applied_of = |applied: bool| {
                    opts.thinking.map(|_| applied)
                };
                if cfg.stream {
                    let mut cb = on_delta;
                    let (full, applied) = client
                        .chat_stream_opts(msgs, opts, |d| {
                            if let Some(f) = cb.as_deref_mut() {
                                f(d);
                            }
                        })
                        .await
                        .map_err(|e| {
                            log::log_line("ai", &format!("AI 请求失败（本机接口）：{e}"));
                            e
                        })?;
                    return Ok(AiOutcome {
                        text: full,
                        thinking_applied: applied_of(applied),
                    });
                }
                let (full, applied) = client.chat_once_opts(msgs, opts).await.map_err(|e| {
                    log::log_line("ai", &format!("AI 请求失败（本机接口）：{e}"));
                    e
                })?;
                if let Some(f) = on_delta {
                    f(&full);
                }
                return Ok(AiOutcome {
                    text: full,
                    thinking_applied: applied_of(applied),
                });
            }
            if source == AiSource::Local {
                return Err(bluelink_ai::AiError::NotConfigured.into());
            }
        }
        if source != AiSource::Local {
            if self.is_connected().await {
                log::log_line(
                    "ai",
                    &format!("AI 请求路由：经对方设备转发（{} 条消息）", msgs.len()),
                );
                return self.proxy_to_peer(msgs, on_delta, opts).await.map_err(|e| {
                    log::log_line("ai", &format!("AI 请求失败（对端转发）：{e}"));
                    e
                });
            }
            if source == AiSource::Peer {
                return Err(ServiceError::NotConnected);
            }
        }
        Err(bluelink_ai::AiError::NotConfigured.into())
    }

    /// AI 页的一轮调用：增量同步转成 AiChunk 事件推给前端。
    /// 思考开关带了却没生效时经 AiThinkingNote 事件让前端如实提示。
    async fn chat_anywhere(
        self: &Arc<Self>,
        conv_id: &str,
        msgs: &[ChatMessage],
        opts: &AiCallOptions,
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
        let outcome = self.chat_core_src(msgs, Some(&mut cb), source, opts).await?;
        if outcome.thinking_applied == Some(false) {
            self.emit(Event::AiThinkingNote);
        }
        Ok(outcome.text)
    }

    /// 本地 API 网关入口：供本机其他软件经 OpenAI 兼容接口调用（见应用层本地 API 服务）。
    /// 路由与 AI 页一致：本机配置优先，否则经已连接设备转发；请求里带的模型名仅作参考，
    /// 实际模型以真正调用上游一侧的配置为准（/v1/models 如实返回该模型名）。
    /// opts 透传调用方软件给的 temperature/max_tokens/top_p（见 local_api 解析）。
    pub async fn gateway_chat(
        &self,
        msgs: &[ChatMessage],
        opts: &AiCallOptions,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String, ServiceError> {
        log::log_line(
            "api",
            &format!("本地 API 网关请求：{} 条消息", msgs.len()),
        );
        let outcome = self
            .chat_core_src(msgs, Some(on_delta), AiSource::Auto, opts)
            .await
            .map_err(|e| {
                log::log_line("api", &format!("本地 API 网关请求失败：{e}"));
                e
            })?;
        Ok(outcome.text)
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
    /// opts（思考开关/网关参数）随 AiProxyRequest 一起带给对端、对端原样用。
    async fn proxy_to_peer(
        &self,
        msgs: &[ChatMessage],
        mut on_delta: Option<&mut (dyn FnMut(&str) + Send)>,
        opts: &AiCallOptions,
    ) -> Result<AiOutcome, ServiceError> {
        let session = self.session().await?;
        let request_id = new_id();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ProxyRecv>(64);
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
                thinking: opts.thinking,
                temperature: opts.temperature,
                max_tokens: opts.max_tokens,
                top_p: opts.top_p,
            }))
            .await?;
        let mut full = String::new();
        let mut thinking_applied: Option<bool> = None;
        while let Some(item) = rx.recv().await {
            match item {
                ProxyRecv::Delta(delta) => {
                    if let Some(f) = on_delta.as_deref_mut() {
                        f(&delta);
                    }
                    full.push_str(&delta);
                }
                ProxyRecv::Done {
                    error,
                    thinking_applied: applied,
                } => {
                    thinking_applied = applied;
                    if let Some(e) = error {
                        self.proxy_waiters.lock().await.remove(&request_id);
                        return Err(ServiceError::Other(e));
                    }
                    break;
                }
            }
        }
        self.proxy_waiters.lock().await.remove(&request_id);
        if full.is_empty() {
            return Err(ServiceError::Other("对方未能提供 AI 代理".into()));
        }
        Ok(AiOutcome {
            text: full,
            thinking_applied,
        })
    }

    async fn handle_ai_proxy_frame(&self, c: Control) {
        match c {
            Control::AiProxyChunk { request_id, delta } => {
                // 空分片不入等待通道（转发端已滤空片，这里再防一手混版对端）。
                if delta.is_empty() {
                    return;
                }
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx.send(ProxyRecv::Delta(delta)).await;
                }
            }
            Control::AiProxyDone {
                request_id,
                error,
                thinking_applied,
            } => {
                if let Some(tx) = self.proxy_waiters.lock().await.get(&request_id) {
                    let _ = tx
                        .send(ProxyRecv::Done {
                            error,
                            thinking_applied,
                        })
                        .await;
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
    /// opts 是请求方随帧带来的请求级选项（思考开关/网关参数），原样用于组装
    /// 上游请求；全程记 essential 日志（v2.6.1 补可见性：到达/路由/上游结果，
    /// 只记条数与状态，绝不记密钥与内容全文）。
    async fn serve_ai_proxy(
        &self,
        session: Arc<PeerSession>,
        request_id: String,
        messages: Vec<serde_json::Value>,
        opts: &AiCallOptions,
    ) -> Result<(), ServiceError> {
        log::log_line(
            "ai",
            &format!(
                "收到 AI 转发请求：{} 条消息（对方 {}）",
                messages.len(),
                session.peer.name
            ),
        );
        let allowed = self
            .storage
            .get_setting("ai_proxy_allow")
            .map(|v| v.as_deref() == Some("1"))
            .unwrap_or(false);
        if !allowed {
            log::log_line("ai", "AI 转发请求被拒：本机未开启 AI 转发许可");
            session
                .conn
                .send(&Frame::Control(Control::AiProxyDone {
                    request_id,
                    error: Some(
                        "对方未开启 AI 转发许可：请让对方在设置的「AI 转发」里打开允许开关".into(),
                    ),
                    thinking_applied: None,
                }))
                .await?;
            return Ok(());
        }
        let cfg = self.ai_cfg.lock().await.clone();
        let Some(cfg) = cfg.filter(|c| c.is_ready()) else {
            log::log_line("ai", "AI 转发请求被拒：本机未配置 AI");
            session
                .conn
                .send(&Frame::Control(Control::AiProxyDone {
                    request_id,
                    error: Some("对方未配置 AI".into()),
                    thinking_applied: None,
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
        let client = AiClient::new(cfg.clone());
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
            .chat_stream_opts(&msgs, opts, |delta| {
                let _ = tx.try_send(delta.to_string());
            })
            .await;
        drop(tx);
        let _ = forwarder.await;
        let (error, thinking_applied) = match &result {
            Ok((full, applied)) => {
                self.add_proxy_stats(prompt_chars + full.chars().count() as u64);
                log::log_line(
                    "ai",
                    &format!(
                        "AI 转发完成：上游模型 {}，回答 {} 字",
                        cfg.model,
                        full.chars().count()
                    ),
                );
                (None, opts.thinking.map(|_| *applied))
            }
            Err(e) => {
                // 上游错误原文截断后记日志（文案含状态码、不含密钥）
                let brief: String = e.to_string().chars().take(120).collect();
                log::log_line("ai", &format!("AI 转发失败：上游返回 {brief}"));
                (Some(e.to_string()), None)
            }
        };
        session
            .conn
            .send(&Frame::Control(Control::AiProxyDone {
                request_id,
                error,
                thinking_applied,
            }))
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
