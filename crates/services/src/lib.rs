//! 应用服务层：把协议、传输、存储、AI 编排成一个可用的节点（Node）。
//! 桌面端（Tauri）与演示 CLI 都只调用这一层，不直接碰底层 crate。

pub mod log;
mod machine_id;
pub use log::log_line;

use bluelink_ai::{AiClient, AiConfig, ChatMessage, RichMessage};
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
        /// 评审补强：状态事件自带 msg_id，前端记账不再依赖 progress
        /// 事件先行到达才能绑气泡（无进度先行的取消/失败也能落定）。
        msg_id: String,
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
    /// 自动重连状态（v2.6.2，派生自 #11）：意外断线后自动重连期间
    /// 发出，前端据此明示「重连中」。state: trying（第 attempt 次尝试）/
    /// connected（已恢复）/ gave_up（总时限到、停手）。
    ReconnectState {
        peer_name: String,
        attempt: u32,
        state: String,
    },
    /// 中继更新的安装包已收齐并通过传输层哈希校验、落入更新暂存
    /// 目录（中继更新功能起）：应用层据此做签名校验并走安装确认。
    /// 注意：此事件只证明字节完整，签名未验——绝不可跳过验签直接装。
    UpdatePackageReady {
        transfer_id: String,
        path: String,
        name: String,
        size: u64,
    },
}

/// 中继更新查到的最新版信息（对端代取 latest.json 后回传）：版本号、
/// 更新说明原文与本平台安装包的签名（收包后验签用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelayUpdateInfo {
    pub version: String,
    pub notes: String,
    pub signature: String,
}

/// 中继取包等待者的应答：（对端本次取包实际所据的版本, 签名）。
type UpdateFetchReply = Result<(Option<String>, Option<String>), String>;

/// 自动重连目标：一次成功出站连接留下的回拨信息（route 是应用层
/// 的拨号路线，如 "bt:ble:<扫描id>" 或 "tcp:<addr>"，服务层不解读、
/// 只原样交给注册进来的拨号器；code 为对端连接码，由设备 ID 派生）。
#[derive(Debug, Clone)]
pub struct ReconnectTarget {
    pub peer_id: String,
    pub name: String,
    pub code: String,
    pub route: String,
}

/// 应用层注册的拨号器：按 route 重新拨号并 attach，成功返回 Ok。
/// gen 为重连代号，拨号器应在 attach 前用 reconnect_should_continue
/// 核对（用户中途主动断开/新连接建立都会让代号失效）。
/// 会话结束的性质（v2.6.2）：决定断开后是否触发自动重连——
/// 只有 Fault（链路结束/心跳判死/发送失败等意外中断）才重连；
/// User（用户主动断开）绝不触发；Replaced（被新连接替换）也不触发；
/// PeerGoodbye（对方发来 Goodbye 告别帧后断开，v2.6.6 #17）与 User
/// 同规：对方是人主动断的，绝不许自动连回去打扰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    User,
    Replaced,
    Fault,
    PeerGoodbye,
}

pub type ReconnectDialer = std::sync::Arc<
    dyn Fn(
            Arc<Node>,
            ReconnectTarget,
            u64,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

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

/// 「打开文件」的处置分类（v2.6.2，#9 安全定版）：打开一律是**查看**
/// 语义，绝不允许点一下就把脚本/程序跑起来。老版一律走系统默认
/// 打开，Windows 上 .py 的默认 verb 就是解释器执行——超 10MB 的
/// 脚本点「打开」被直接运行（v2.6.1 验后 #9 实锤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenClass {
    /// 文本/代码/脚本：内置查看器（只看不跑，任意大小降级打开）。
    Viewer,
    /// 二进制可执行类：只在文件夹中显示，永不系统打开。
    Reveal,
    /// 其余文档/媒体类：系统默认程序打开（查看语义成立的格式）。
    System,
}

/// 按扩展名分类（小写比较）。处置对照表同步写在发布说明里。
pub fn classify_open(name: &str) -> OpenClass {
    let ext = name
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty() && !e.contains('/'))
        .unwrap_or("")
        .to_ascii_lowercase();
    // 文本/代码/脚本（含 bat/cmd/ps1/sh 等脚本）：查看器只读渲染
    const TEXT_EXTS: &[&str] = &[
        "txt",
        "log",
        "md",
        "markdown",
        "rs",
        "py",
        "js",
        "ts",
        "tsx",
        "jsx",
        "java",
        "c",
        "h",
        "cpp",
        "hpp",
        "cc",
        "cs",
        "go",
        "rb",
        "pl",
        "php",
        "lua",
        "swift",
        "kt",
        "scala",
        "sh",
        "bash",
        "zsh",
        "bat",
        "cmd",
        "ps1",
        "psm1",
        "vbs",
        "sql",
        "ms",
        "json",
        "jsonl",
        "toml",
        "yaml",
        "yml",
        "xml",
        "html",
        "htm",
        "css",
        "scss",
        "vue",
        "ini",
        "cfg",
        "conf",
        "csv",
        "tsv",
        "env",
        "gitignore",
        "dockerfile",
        "makefile",
        "gradle",
        "properties",
        "r",
        "m",
        "mm",
        "asm",
        "s",
        "dart",
        "ex",
        "exs",
        "erl",
        "hs",
        "clj",
        "groovy",
    ];
    // 二进制可执行/安装包/快捷方式：只定位显示，永不打开
    const EXEC_EXTS: &[&str] = &[
        "exe", "msi", "com", "scr", "cpl", "dll", "sys", "drv", "ocx", "jar", "apk", "ipa", "dmg",
        "pkg", "deb", "rpm", "appimage", "run", "bin", "lnk", "url", "app", "gadget", "msc", "reg",
        "jse", "wsf", "wsh", "vbe", "pif",
    ];
    // 无扩展名但众所周知的文本文件（评审补强）：rsplit 对无点名返回
    // 整个文件名，Dockerfile/Makefile 本就在 TEXT_EXTS 里；点文件与
    // 说明文件在此兜底，免得落进 System 被系统打开。
    const PLAIN_TEXT_NAMES: &[&str] = &[
        ".bashrc",
        ".zshrc",
        ".profile",
        ".gitignore",
        ".gitattributes",
        ".editorconfig",
        "license",
        "readme",
        "changelog",
        "authors",
        "todo",
    ];
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    if PLAIN_TEXT_NAMES.contains(&base.to_ascii_lowercase().as_str()) {
        return OpenClass::Viewer;
    }
    if TEXT_EXTS.contains(&ext.as_str()) {
        OpenClass::Viewer
    } else if EXEC_EXTS.contains(&ext.as_str()) {
        OpenClass::Reveal
    } else {
        OpenClass::System
    }
}

/// 打开文件的实际结果：调用方据此决定是走内置查看器还是已处理完。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// 应由内置查看器打开（命令层转告前端）。
    Viewer,
    /// 已处理（系统打开或文件夹定位）。
    Done,
}

/// 打开文件（#9 定版）：文本/脚本 → 返回 Viewer 交内置查看器；
/// 可执行类 → 只在文件夹中定位；其余 → 系统默认程序。
/// 任何路径下都不会以默认 verb 打开脚本/可执行文件。
pub fn open_file(path: &Path) -> Result<OpenOutcome, ServiceError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    match classify_open(&name) {
        OpenClass::Viewer => Ok(OpenOutcome::Viewer),
        OpenClass::Reveal => {
            reveal_file(path)?;
            Ok(OpenOutcome::Done)
        }
        OpenClass::System => {
            open_with_system(path)?;
            Ok(OpenOutcome::Done)
        }
    }
}

/// 用系统默认程序打开（仅供 classify_open 判为 System 的文件与网址）。
fn open_with_system(path: &Path) -> Result<(), ServiceError> {
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
    open_with_system(std::path::Path::new(url))
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

/// 粘贴/发送准备路径上剪贴板读取的总时长上限：剪贴板被其他程序长
/// 时间占用、或内容所有者对延迟渲染迟迟不响应时，系统读取调用会
/// 无限期阻塞——用户实测长文本「发送中」十几分钟、文件真正传输只
/// 用 698ms，卡的就是这一步（诊断见 goal hidden_files 的
/// clipboard-lock-diagnosis-2026-11-25）。这条路径上的读取一律设
/// 截止：超时/被占用即降级，绝不无限等待。
pub const CLIPBOARD_READ_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// 在独立线程里执行闭包并设总时长上限：超时返回 None，调用方降级。
/// 卡住的线程留在系统调用里自生自灭——它只持有自己新建的短句柄、
/// 不碰 Node 的长驻句柄与任何共享锁，拖不垮其他路径；这类线程只
/// 由用户粘贴操作触发（每次至多一条），不会累积失控。
pub fn call_with_deadline<T: Send + 'static>(
    budget: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(budget).ok()
}

/// 一次带截止的剪贴板图片读取结果：拿到图 / 确实没有图 / 降级
/// （超时或被占用——调用方按「没有图片」走，但必须留痕定责）。
enum ClipImageRead {
    Image(arboard::ImageData<'static>),
    None,
    Degraded(&'static str),
}

/// 用独立短句柄在截止内读一次剪贴板图片（粘贴路径专用：不碰 Node
/// 长驻句柄与 clip_handle 锁，剪贴板锁死时也不会排队等待）。
fn read_clipboard_image_bounded() -> ClipImageRead {
    match call_with_deadline(CLIPBOARD_READ_BUDGET, || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(_) => return ClipImageRead::None,
        };
        match cb.get_image() {
            Ok(img) => ClipImageRead::Image(img),
            Err(arboard::Error::ClipboardOccupied) => {
                ClipImageRead::Degraded("剪贴板被其他程序占用")
            }
            Err(_) => ClipImageRead::None,
        }
    }) {
        Some(r) => r,
        None => ClipImageRead::Degraded("读取超时（剪贴板无响应）"),
    }
}

/// 一次性读系统剪贴板里的原始图像，编码为 PNG 字节返回（输入框粘贴
/// 的图像回落路线）：截图、画图/浏览器里复制的图片在剪贴板里只有
/// 位图数据、没有文件形态，前端 WebView 的 DataTransfer 在部分平台
/// （Windows/WebView2 实测）只给一个取不到数据的 file 占位，靠它
/// 拿不到图——权威来源与文件列表同口径：直接读系统剪贴板。
/// 没有图片、剪贴板被占用或平台读不到 → Ok(None)，不算失败：
/// 它只是文件路线之后的回落，两条都没有才由前端提示。
/// 与 clip_read_with 不同，这里是短命句柄「只读一次」：X11 上读
/// 选择内容不依赖本进程持有，读完即放，不碰 Node 的长驻句柄；
/// 读取本身带 500ms 截止（见 CLIPBOARD_READ_BUDGET），被占用/
/// 超时降级为「没有图片」并打 essential 日志，绝不拖住粘贴。
pub fn clipboard_image_png() -> Result<Option<Vec<u8>>, ServiceError> {
    let img = match read_clipboard_image_bounded() {
        ClipImageRead::Image(i) => i,
        ClipImageRead::None => return Ok(None),
        ClipImageRead::Degraded(why) => {
            log::log_line(
                "clipboard",
                &format!("粘贴读取剪贴板图片降级：{why}，按没有图片处理（文字粘贴不受影响）"),
            );
            return Ok(None);
        }
    };
    let rgba =
        image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())
            .ok_or_else(|| ServiceError::Other("剪贴板图片尺寸异常".into()))?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| ServiceError::Other(format!("图片编码失败：{e}")))?;
    Ok(Some(buf.into_inner()))
}

/// 剪贴板里是否有图片（只查存在性，不解码、不编码、不落盘）：供 AI 框
/// 粘贴图片时补判。旧命令曾复用 clipboard_image_png 的完整 PNG 编码，
/// 只为判存在性白白编一次大图（第三方评审）；这里只读一次 get_image
/// 的成败，图大也不编码。读取带 500ms 截止：被占用/超时降级为
/// 「没有图片」并打 essential 日志，绝不拖住粘贴。
pub fn clipboard_has_image() -> Result<bool, ServiceError> {
    match read_clipboard_image_bounded() {
        ClipImageRead::Image(_) => Ok(true),
        ClipImageRead::None => Ok(false),
        ClipImageRead::Degraded(why) => {
            log::log_line(
                "clipboard",
                &format!("粘贴探测剪贴板图片降级：{why}，按没有图片处理"),
            );
            Ok(false)
        }
    }
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

/// 已备好的待发文件（消息已落库上屏、尚未开传）：prepare_file_send
/// 的产物，send_file / send_files_batch 据此跑传输并收尾。
struct PreparedFileSend {
    msg_id: String,
    name: String,
    size: u64,
    sha: String,
    transfer_id: String,
    path: PathBuf,
}

pub fn derive_transfer_id(file_sha: &str, peer_id: &str) -> String {
    let d = Sha256::digest(format!("{file_sha}:{peer_id}").as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]
    )
}

/// 接收文件按日分夹（v2.6.5，#13）：接收完成的日期（UTC+8，与本应用
/// 既有日期口径一致，见 proxy_day_key）拼成 YYYY-MM-DD 子目录名。
/// days→年月日 用 Howard Hinnant 的 civil-from-days 算法，无需引日期库。
fn receive_date_dir_name(now: i64) -> String {
    let days = (now + 8 * 3600 * 1000).div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
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

/// 系统剪贴板序号（仅 Windows）：每次有人往剪贴板放东西就 +1，
/// 同一份内容重新复制也会变——靠它区分「内容没变」与「用户又复制
/// 了一遍」（复测 #7：A→A 第二份要算新复制）。非 Windows 平台没有
/// 等价接口，返回 None、沿用纯内容口径。
#[cfg(windows)]
fn clipboard_seq() -> Option<u32> {
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}
#[cfg(not(windows))]
fn clipboard_seq() -> Option<u32> {
    None
}

/// 一次剪贴板读取的结果：内容（可能为空）+ 是否因「被其他程序占用」读不到。
/// 「剪贴板里没东西」与「被占用读不了」必须分开——前者是常态、后者是
/// 故障信号（v2.6.1 前两者都静默成 None，出问题无据可查）。
struct ClipRead {
    content: Option<ClipContent>,
    occupied: bool,
    /// 读取时刻的系统剪贴板序号（见 clipboard_seq）。
    seq: Option<u32>,
}

/// 读系统剪贴板：与粘贴规则同口径——先文字，纯图片才取图片；空剪贴板返回 None。
/// 句柄由 Node 长驻持有（见 clip_handle 字段注释），不要在这里新建临时句柄。
fn clip_read_with(cb: &mut arboard::Clipboard) -> ClipRead {
    let seq = clipboard_seq();
    let mut occupied = false;
    match cb.get_text() {
        Ok(t) => {
            if !t.is_empty() {
                return ClipRead {
                    content: Some(ClipContent::Text(t)),
                    occupied: false,
                    seq,
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
                seq,
            };
        }
        Err(arboard::Error::ClipboardOccupied) => occupied = true,
        Err(_) => {}
    }
    ClipRead {
        content: None,
        occupied,
        seq,
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

/// 剪贴板当前所有者所属进程的可执行文件名（仅 Windows 实现）：
/// 读到「被占用」时点名占用者，省得再靠猜（v1.0.4 #6 诊断）。
/// 注意 GetClipboardOwner 给的是「最后往剪贴板放数据的窗口」，
/// 与「此刻正打开剪贴板读写的进程」不完全是一回事——日志措辞
/// 按「占用者」记，定性时留有余地；拿不到就返回 None 不硬凑。
#[cfg(windows)]
fn clipboard_owner_desc() -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::DataExchange::GetClipboardOwner;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe {
        let hwnd = GetClipboardOwner();
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if proc.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(proc);
        if ok == 0 || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        Some(format!("{name}（PID {pid}）"))
    }
}

/// 非 Windows 平台暂无所有者查询实现。
#[cfg(not(windows))]
fn clipboard_owner_desc() -> Option<String> {
    None
}

/// 远端内容写入本机剪贴板的结果（v2.6.5，#8 定案）。
enum ClipApplyOutcome {
    /// 已写入并回读验证生效。readback_hash 是回读内容的哈希——
    /// 经系统格式转换后它可能与线缆哈希不同，#16 应用登记要把它
    /// 一并钉死，poll 才认得出「这是同步来的」。
    Written { readback_hash: Option<String> },
    /// 本机当前内容与远端相同：不重复写（重复写会在系统剪贴板历史里
    /// 把同一内容记两遍——用户实报的「历史出现两遍」）。
    SkippedSame,
    /// 本机在上次轮询之后刚被用户复制了新内容（与监视记录对不上）：
    /// 本机用户的最新复制优先，本条远端内容让行（同步绝不顶掉用户
    /// 刚复制的东西，红线）。
    SkippedLocalNewer,
    /// 写入失败/未生效：已尽力还原写入前的内容，调用方进退让期。
    Failed(String),
}

/// 把远端剪贴板内容原子地应用到本机（读→比→写→验，全程同一句柄
/// 一次完成，关掉「比对完、写入前用户又复制了」的竞态窗口）：
/// ① 当前内容与远端相同就不写；② 当前内容是用户刚复制的新东西
/// 就让行；③ 写入后回读验证——Windows 上 set_image 的格式转换在
/// EmptyClipboard 之后失败会把剪贴板留成空的（用户实报「剪贴板被
/// 弄空、Ctrl+V 失效、须去历史里点一下才恢复」的机制定案），验证
/// 不过立刻把写入前的快照原样写回，绝不把空剪贴板留给用户。
/// 返回 (结果， 本次实际调用系统写入的次数）——#16 写入计数埋点：
/// 调用方把次数累加进 ClipGuard.write_count 并落日志，真机对账
/// 「剪贴板历史多一条」时能逐次定位是哪一次写入干的。
fn clip_apply_atomic(
    cb: &mut arboard::Clipboard,
    content: &ClipContent,
    last_seen: Option<&str>,
) -> (ClipApplyOutcome, u64) {
    let mut writes = 0u64;
    let cur = clip_read_with(cb);
    if cur.occupied && cur.content.is_none() {
        return (
            ClipApplyOutcome::Failed("剪贴板被其他程序占用".into()),
            writes,
        );
    }
    let snapshot = cur.content;
    if let Some(c) = &snapshot {
        // 等价即算同（评审补强）：本机已有 \r\n 版、线上来 \n 版
        // 时严格哈希会落进 SkippedLocalNewer 且不登记，下一拍有
        // 被当成本机新复制回传的风险；等价判定与 poll 侧同口径。
        if clip_equiv(c, content) {
            return (ClipApplyOutcome::SkippedSame, writes);
        }
        if last_seen != Some(c.hash().as_str()) {
            return (ClipApplyOutcome::SkippedLocalNewer, writes);
        }
    }
    writes += 1;
    if let Err(e) = clip_write_with(cb, content) {
        // 写失败也可能已把剪贴板清空（先 Empty 后失败）：有快照就还原
        if let Some(snap) = &snapshot {
            writes += 1;
            let _ = clip_write_with(cb, snap);
        }
        return (ClipApplyOutcome::Failed(e), writes);
    }
    // 回读验证：文字比等价（行尾可能被系统改成 \r\n，哈希全等会误判，
    // #16）；图片只比尺寸（RGBA 经系统格式往返不保证逐字节一致）。
    // 读回空/不符 = 写入未生效 → 还原快照。
    let back = clip_read_with(cb);
    let verified = match (&content, &back.content) {
        (ClipContent::Text(_), Some(ClipContent::Text(_))) => back
            .content
            .as_ref()
            .map(|c| clip_equiv(c, content))
            .unwrap_or(false),
        (
            ClipContent::Image { width, height, .. },
            Some(ClipContent::Image {
                width: w2,
                height: h2,
                ..
            }),
        ) => w2 == width && h2 == height,
        _ => false,
    };
    if !verified {
        if let Some(snap) = &snapshot {
            writes += 1;
            let _ = clip_write_with(cb, snap);
        }
        return (
            ClipApplyOutcome::Failed("写入后回读验证未通过（已还原原内容）".into()),
            writes,
        );
    }
    let readback_hash = back.content.as_ref().map(|c| c.hash());
    (ClipApplyOutcome::Written { readback_hash }, writes)
}

/// 文字等价归一化：Windows 剪贴板文字在格式往返中常被改成 \r\n，
/// 与线上原文哈希对不上——等价判定前先把行尾统一成 \n。
fn clip_normalize_text(t: &str) -> String {
    t.replace("\r\n", "\n").replace('\r', "\n")
}

/// clip_handle 锁的最长等待：正常持有只有一次读写的毫秒级，等不到
/// 只可能是持有者卡在系统调用里——等满即放弃，绝不无限排队。
const CLIP_LOCK_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// 带截止地拿一把 std Mutex（try_lock 自旋）：超时或锁毒化返回 None。
fn lock_with_deadline<T>(
    m: &std::sync::Mutex<T>,
    budget: std::time::Duration,
) -> Option<std::sync::MutexGuard<'_, T>> {
    let start = std::time::Instant::now();
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(_)) => return None,
            Err(std::sync::TryLockError::WouldBlock) => {
                if start.elapsed() >= budget {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// 两份剪贴板内容是否「同一份」（等价判定，v2.6.6 #16）：
/// 系统剪贴板格式转换会改字节不改内容——文字可能换行尾、图片可能
/// 动 alpha 通道或个别字节。哈希全等只是等价的特例；防回环必须按
/// 等价认人，否则转换后的读回内容会被当成新复制、原样回传给对方
/// （2026-11-22 双机日志定案：同步内容被原样回传 + 同内容重复发送，
/// v2.6.5 的 suppress 按线缆哈希登记、与读回哈希对不上，未闭环）。
fn clip_equiv(a: &ClipContent, b: &ClipContent) -> bool {
    match (a, b) {
        (ClipContent::Text(x), ClipContent::Text(y)) => {
            clip_normalize_text(x) == clip_normalize_text(y)
        }
        (
            ClipContent::Image {
                width: w1,
                height: h1,
                rgba: r1,
            },
            ClipContent::Image {
                width: w2,
                height: h2,
                rgba: r2,
            },
        ) => {
            if w1 != w2 || h1 != h2 || r1.len() != r2.len() {
                return false;
            }
            if r1 == r2 {
                return true;
            }
            // 容忍：alpha 通道（每 4 字节第 4 字节）任意差异 + 其余
            // 字节差异 ≤ 0.1%，且小图另按 5% 封顶（下限 2 字节）——
            // 原下限 8 字节对 4x3 小图等于放行 1/6 像素改色，相近
            // 小图标会被误判同一份、同步静默丢失（评审补强）。行
            // 翻转/尺寸变化等真差异仍远超此界，不会误认。
            let budget = (r1.len() / 1000).max(2).min(r1.len() / 20 + 1);
            let mut diff = 0usize;
            for (i, (x, y)) in r1.iter().zip(r2.iter()).enumerate() {
                if x != y {
                    if i % 4 == 3 {
                        continue; // alpha 通道不计
                    }
                    diff += 1;
                    if diff > budget {
                        return false;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// 一次「从对方写入本机」的应用登记（#16）：线缆内容本体 + 线缆
/// 哈希 + 写入后回读哈希，三样都留——poll 读到的本机内容无论与哪
/// 一样对上（或与本体等价），都认作同步来的，绝不回发。
struct AppliedClip {
    content: ClipContent,
    wire_hash: String,
    readback_hash: Option<String>,
}

/// 手动同步一次的结果（快捷键/设置页按钮触发）：界面据此给人话提示，
/// 被拦下的每种情况在服务层都已打 essential 日志。
#[derive(Debug, Clone, PartialEq)]
pub enum ManualSyncOutcome {
    /// 已发出最新 N 条。
    Sent(usize),
    /// 尚未连接设备。
    NotConnected,
    /// 本机剪贴板同步未开启。
    LocalOff,
    /// 对方未开启剪贴板同步。
    PeerOff,
    /// 没攒到可同步的内容（或全部超限/发送失败）。
    Empty,
    /// 手动触发时现读剪贴板被其他程序占用：这次没读到新内容、
    /// 也没拿旧存货冒充（v1.0.4 #6）。
    Occupied,
}

/// poll 一次读到本机内容后的裁决（#16 状态机，纯逻辑可单测）。
enum ClipPollDecision {
    /// 与本机基线相同：无变化。
    Unchanged,
    /// 是同步写入的内容（或其格式转换形态）：只更新基线，绝不发送。
    SyncEcho,
    /// 本机用户的新复制：发送给对方。
    Send,
}

/// apply 收到对方内容时的裁决（#16 状态机，纯逻辑可单测）。
enum ClipApplyDecision {
    /// 是本机发出去过的内容被回传：回声，丢弃不写（写了会在系统
    /// 剪贴板历史里把同一内容记第二遍）。
    EchoDrop,
    /// 本机已经是这份内容（或刚应用过等价内容）：不重复写。
    AlreadyLocal,
    /// 真新内容：走原子写入。
    Proceed,
}

/// 剪贴板同步的防回环状态机（v2.6.6，#16 三本账重构）：
/// - local_seen：本机基线——poll 最近在本机读到的内容哈希，只由
///   prime/poll/apply 回读登记更新，语义是「本机剪贴板现在是什么」；
/// - sent：发送登记（有界 FIFO）——本机发给对方过的内容哈希，
///   apply 收到命中即判回声，永不被接收事件冲掉（旧版单槽 suppress
///   正是被冲掉才漏）；
/// - applied：应用登记——最近一次从对方写入的内容（本体+双哈希），
///   poll 按等价认人，格式转换后的读回也认得出。
///
/// write_backoff_until / last_read_fail_log 是 v2.6.1 的退让与限频
/// 字段：同步绝不许影响本机功能——写不进去立刻退让、不许重试风
/// 暴；读失败要留痕但不许刷屏。
#[derive(Default)]
struct ClipGuard {
    local_seen: Option<String>,
    /// 与 local_seen 配套的系统剪贴板序号（复测 #7）：基线内容上次
    /// 被裁决/发送/观察时剪贴板的序号。内容哈希没变但序号变了 =
    /// 用户把同一份内容又复制了一遍，按新复制处理（它就是现在的
    /// 最新一条）。只由 poll 裁决的回声/无变化分支、prime 与发送
    /// 成功后的 note_seq 推进；裁决判 Send 时不推进——发送失败或
    /// 图片延后时下一拍凭序号差继续判 Send，不丢重试。
    local_seq: Option<u32>,
    sent: std::collections::VecDeque<String>,
    applied: Option<AppliedClip>,
    /// 写剪贴板失败后的退让截止时刻：此前到达的远端内容直接丢弃不再尝试写。
    write_backoff_until: Option<std::time::Instant>,
    /// 上次记录「读剪贴板被占用」的时刻（限频：每分钟至多一条）。
    last_read_fail_log: Option<std::time::Instant>,
    /// 剪贴板图片因出站繁忙被延后的次数（v2.6.5，#18）：BLE 家族上大图
    /// 是背景流量，出站队列有货时先让行聊天/控制帧，延后不丢、下一拍
    /// 再试；连续延后到上限后照常发送，不许饿死。
    clip_defer_count: u32,
    /// 正在延后的图片哈希（v2.6.5 评审补强）：延后期间用户复制了新图
    /// （哈希变了）要把延后计数清零重新让行，不能继承旧图的计数。
    clip_defer_hash: Option<String>,
    /// 本进程经同步路径实际写入系统剪贴板的累计次数（#16 埋点）：
    /// 真机再现「历史多一条」时，日志里第 N 次写入可逐次对账。
    write_count: u64,
    /// 本机剪贴板历史缓冲（多条/手动同步用）：只存内存、进程退出即
    /// 清，绝不落盘/入 DB。只收 poll 裁决为「本机新复制」的内容与
    /// 手动模式下 prime 的基线，远端应用来的内容绝不入列（那是回声）。
    history: std::collections::VecDeque<ClipContent>,
    /// 最近应用过的远端内容哈希账（线缆哈希与回读哈希都记）：多条
    /// 依次写入时单槽 applied 只记得住最后一条，这本账覆盖整批——
    /// poll 侧命中即认回声、绝不回发整批中的任何一条（apply 侧自
    /// 复测 #6 起退出这本账：剪贴板已换内容时对方重发旧内容要写）。
    applied_recent: std::collections::VecDeque<String>,
}

/// 发送登记容量：回声最迟隔一轮往返，16 条足够；超限挤掉最旧的。
const CLIP_SENT_CAP: usize = 16;

/// 本机历史缓冲容量（仅内存）：「最新 5 条」设置上限的两倍余量。
const CLIP_HISTORY_CAP: usize = 10;

/// 应用哈希账容量：覆盖整批（最大 5 条 × 线缆+回读两哈希）再留余量。
const CLIP_APPLIED_CAP: usize = 16;

impl ClipGuard {
    /// 发送/观察落实后推进序号基线（与 note_sent/note_observed 分开
    /// 记：序号的推进点是「这份内容已经处理完」，不是裁决时刻）。
    fn note_seq(&mut self, seq: Option<u32>) {
        if seq.is_some() {
            self.local_seq = seq;
        }
    }

    /// poll 裁决：读到的内容是没变、同步回声、还是用户新复制。
    /// SyncEcho 时顺手把基线推进到读到的哈希（格式转换形态从此成本机基线）。
    /// seq 为系统剪贴板序号（非 Windows 为 None）：内容与基线同哈希
    /// 但序号变了，是用户把同一份内容重新复制了一遍（复测 #7），
    /// 判 Send 让它重新成为最新一条；回声判定排在它前面——同步写
    /// 入同样会顶高序号，绝不能把回声误判成重复制。
    fn poll_decide(&mut self, content: &ClipContent, seq: Option<u32>) -> ClipPollDecision {
        let hash = content.hash();
        // 回声判定优先于一切（含下面的重复制判定）：经同步到达本
        // 机的内容，无论序号怎么变、中间复制过什么，都绝不回发
        // ——用户定案的方向规矩：机器1 复制 A 同步到机器2，机器2
        // 上的 A 永远不许回到机器1（账本有界：超容被挤出的老条目
        // 靠对方 sent 账的 EchoDrop 兜底，最坏是一次同内容重写、
        // 不成环）。原机 A→B→A 第三份发不出去的真因不在这里，
        // 在接收侧 apply_decide 的应用账（见彼处）。
        if let Some(ap) = &self.applied {
            if ap.wire_hash == hash
                || ap.readback_hash.as_deref() == Some(hash.as_str())
                || clip_equiv(content, &ap.content)
            {
                self.local_seen = Some(hash);
                self.note_seq(seq);
                return ClipPollDecision::SyncEcho;
            }
        }
        // 整批应用账：多条同步依次写入时，早期条目可能已被单槽
        // applied 挤掉，但只要还在账里就仍是同步来的，绝不回发。
        if self.applied_recent.contains(&hash) {
            self.local_seen = Some(hash);
            self.note_seq(seq);
            return ClipPollDecision::SyncEcho;
        }
        if self.local_seen.as_deref() == Some(hash.as_str()) {
            // 内容与基线同哈希，但系统序号变了 = 用户把同一份内容
            // 又复制了一遍（复测 #7）：它就是现在的最新一条，按新
            // 复制判 Send（对方已有同份时接收侧会判已有、不重写）。
            // 序号基线还没立（None）时先立基线，不误发。
            match (seq, self.local_seq) {
                (Some(s), Some(prev)) if s != prev => return ClipPollDecision::Send,
                (Some(_), None) => {
                    self.note_seq(seq);
                    return ClipPollDecision::Unchanged;
                }
                _ => return ClipPollDecision::Unchanged,
            }
        }
        ClipPollDecision::Send
    }

    /// 记入本机历史缓冲：与最新一条同哈希不重复记，超容挤最旧。
    fn note_history(&mut self, content: ClipContent) {
        if let Some(last) = self.history.back() {
            if last.hash() == content.hash() {
                return;
            }
        }
        self.history.push_back(content);
        while self.history.len() > CLIP_HISTORY_CAP {
            self.history.pop_front();
        }
    }

    /// 取历史里最新 n 条（时间正序：旧→新；发送时依次发，对方按序
    /// 写入、最终停在最新一条）。
    fn history_latest(&self, n: usize) -> Vec<ClipContent> {
        let skip = self.history.len().saturating_sub(n);
        self.history.iter().skip(skip).cloned().collect()
    }

    /// 手动模式的观察登记：只推进基线，不进发送登记（还没发出去，
    /// 发出时由 send_clip_contents 走 note_sent 登记）。
    fn note_observed(&mut self, hash: String) {
        self.local_seen = Some(hash);
    }

    /// 发送成功后登记（基线 + 发送登记同记）。
    fn note_sent(&mut self, hash: String) {
        self.local_seen = Some(hash.clone());
        if !self.sent.contains(&hash) {
            self.sent.push_back(hash);
            while self.sent.len() > CLIP_SENT_CAP {
                self.sent.pop_front();
            }
        }
    }

    /// apply 裁决：回声/本机已有/真新内容。
    fn apply_decide(&self, content: &ClipContent) -> ClipApplyDecision {
        let hash = content.hash();
        // 本机发出去过的内容被回传：无条件丢弃——这是接收侧的
        // 防回环墙（与 poll 侧回声抑制配套，用户定案的方向规矩）。
        if self.sent.contains(&hash) {
            return ClipApplyDecision::EchoDrop;
        }
        // 剪贴板此刻正装着这份内容：真已有，不重写。
        if self.local_seen.as_deref() == Some(hash.as_str()) {
            return ClipApplyDecision::AlreadyLocal;
        }
        // 应用登记只在本机剪贴板还停在那份应用内容上时才算已有：
        // 基线已被新内容推进过，对方重发旧内容是真更新（复测 #6
        // 真因：原机 A→B→A，第三份 A 到达时接收机停在 B，旧口径
        // 凭「应用账里有 A」判已有丢弃，B 永远变不回 A）。整批账
        // 同理退出 apply 裁决——剪贴板装着同哈希已被上一条覆盖，
        // 其余情形一律照常写入（重写同份内容无害、终点状态正确）。
        if let Some(ap) = &self.applied {
            let baseline_on_applied = self.local_seen.as_deref() == Some(ap.wire_hash.as_str())
                || self.local_seen.as_deref() == ap.readback_hash.as_deref();
            if baseline_on_applied && (ap.wire_hash == hash || clip_equiv(content, &ap.content)) {
                return ClipApplyDecision::AlreadyLocal;
            }
        }
        ClipApplyDecision::Proceed
    }

    /// 写入成功（或本机已是该内容）后登记应用账：本体 + 线缆哈希 +
    /// 回读哈希一并钉死；基线推进到回读哈希（poll 接下来读到的就是它）。
    /// 线缆/回读两哈希同时进整批账（applied_recent），多条依次写入时
    /// 每条都留痕，不只最后一条。
    fn note_applied(&mut self, content: ClipContent, readback_hash: Option<String>) {
        let wire_hash = content.hash();
        self.local_seen = Some(readback_hash.clone().unwrap_or_else(|| wire_hash.clone()));
        for h in std::iter::once(wire_hash.clone()).chain(readback_hash.iter().cloned()) {
            if !self.applied_recent.contains(&h) {
                self.applied_recent.push_back(h);
            }
        }
        while self.applied_recent.len() > CLIP_APPLIED_CAP {
            self.applied_recent.pop_front();
        }
        self.applied = Some(AppliedClip {
            content,
            wire_hash,
            readback_hash,
        });
    }
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
    /// 中继更新安装包：收齐后落更新暂存目录、不建消息不进接收目录。
    update_pkg: bool,
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
            tokens += ascii_run.div_ceil(4) + 1;
            ascii_run = 0;
        }
    }
    tokens + ascii_run.div_ceil(4)
}

/// ㉓ 预算裁剪（纯函数，组装与测试共用）：bodies 为正序历史正文，返回
/// 保留段的起始下标——从最新往旧累加估算 token（每条 +4 结构开销），
/// 装满预算为止；最新一条无条件保留（最新一轮不许截断）。
/// AI 文本附件入参（v1.0.1）：name 是展示文件名，content 是已解码的
/// 文本全文。前端入队时已按同一组上限校验（类型/单件/总量/件数），
/// ai_push_and_answer 里还有第二道闸防绕过；超限整条拒绝、绝不截断。
pub struct AiAttachmentIn {
    pub name: String,
    pub content: String,
}
pub const AI_ATT_MAX_SINGLE: usize = 512 * 1024;
pub const AI_ATT_MAX_TOTAL: usize = 512 * 1024;
pub const AI_ATT_MAX_COUNT: usize = 5;

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
    /// 对方已发来 Goodbye 告别帧（#17）：本会话无论从哪条路径收尾
    /// （接收循环 EOF/心跳判死），都按「对方主动断开」分类、不重连。
    peer_goodbye: AtomicBool,
    /// 握手时对端自报的应用版本（旧版本为空串）。
    peer_app_version: String,
    /// 握手时对端自报的中继更新能力位（旧版本恒 false）。
    peer_relay_update: bool,
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
    /// 中继更新等待者：查版本（req_id → 一次性应答通道）与取包
    /// （req_id → 对端下载成败通道；包本体走文件传输事件，不经此表）。
    update_check_waiters: Mutex<HashMap<String, oneshot::Sender<Result<RelayUpdateInfo, String>>>>,
    update_fetch_waiters: Mutex<HashMap<String, oneshot::Sender<UpdateFetchReply>>>,
    /// 更新暂存目录（数据目录下 updates）：中继收到的安装包落点与
    /// 本机中继下载时的临时落点（传完即删，不留缓存——用户定案）。
    updates_dir: PathBuf,
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
    /// 自动重连（v2.6.2）：上次成功出站连接留下的回拨目标。
    reconnect_target: Mutex<Option<ReconnectTarget>>,
    /// 应用层在 attach 前登记的本次拨号路线（attach 成功时被消费）。
    pending_route: Mutex<Option<String>>,
    /// 重连代号：新连接建立、用户主动断开都会自增，旧重连循环据此停手。
    reconnect_gen: std::sync::atomic::AtomicU64,
    /// 是否已有重连循环在跑（防重复拉起）。
    reconnect_active: AtomicBool,
    /// 应用层注册的拨号器（未注册时不自动重连，如纯服务层测试场景）。
    reconnect_dialer: Mutex<Option<ReconnectDialer>>,
    /// #17 重连抑制窗：收到 Goodbye 的对端 peer_id → 抑制截止时刻。
    /// 收到告别帧即登记（10 分钟），maybe_start_reconnect 命中未过期
    /// 条目直接不启动；attach 成功（用户手动连上）即清除。兜底防线：
    /// 即使会话分类在某条竞态路径上漏标，抑制窗也能拦下自动重连。
    reconnect_suppress: Mutex<HashMap<String, std::time::Instant>>,
    /// #17 冷静期兜底：peer_id → （窗口起始时刻， 120 秒窗内
    /// 连续 Fault 次数）。告别帧没送达（旧版对端/链路已死）时，
    /// 反复断开-重连的循环靠它逐步拉长首次重连等待，不许 2 秒一撞。
    fault_history: Mutex<HashMap<String, (std::time::Instant, u32)>>,
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
            update_check_waiters: Mutex::new(HashMap::new()),
            update_fetch_waiters: Mutex::new(HashMap::new()),
            updates_dir: data_dir.join("updates"),
            recv_files: Mutex::new(HashMap::new()),
            proxy_waiters: Mutex::new(HashMap::new()),
            outbound_mark: Mutex::new(None),
            glare_yield_gen: Mutex::new(None),
            outbound_gen: std::sync::atomic::AtomicU64::new(0),
            cancelled: Mutex::new(HashSet::new()),
            clip_peer_on: AtomicBool::new(false),
            clip_guard: Mutex::new(ClipGuard::default()),
            clip_handle: std::sync::Mutex::new(None),
            reconnect_target: Mutex::new(None),
            pending_route: Mutex::new(None),
            reconnect_gen: std::sync::atomic::AtomicU64::new(0),
            reconnect_active: AtomicBool::new(false),
            reconnect_dialer: Mutex::new(None),
            reconnect_suppress: Mutex::new(HashMap::new()),
            fault_history: Mutex::new(HashMap::new()),
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
    ) -> Result<(Device, String, bool), ServiceError> {
        let (peer, peer_app_version, peer_relay_update) = if host {
            // 宿主侧：等对方 Hello，校验连接码
            log::log_verbose("conn", "握手步骤5/6：等待对方 Hello");
            match conn.recv().await? {
                Frame::Control(Control::Hello {
                    device_id,
                    device_name,
                    code,
                    app_version,
                    relay_update,
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
                                app_version: env!("CARGO_PKG_VERSION").to_string(),
                                relay_update: true,
                            }))
                            .await;
                        return Err(ServiceError::BadCode);
                    }
                    conn.send(&Frame::Control(Control::HelloAck {
                        device_id: self.device_id.clone(),
                        device_name: self.device_name.clone(),
                        accepted: true,
                        reason: None,
                        app_version: env!("CARGO_PKG_VERSION").to_string(),
                        relay_update: true,
                    }))
                    .await?;
                    log::log_verbose(
                        "conn",
                        &format!("握手步骤6/6：连接码校验通过，已回 HelloAck，握手完成（对方 {device_name}）"),
                    );
                    (
                        Device {
                            device_id,
                            name: device_name,
                            code: String::new(),
                            last_seen: now_ms(),
                        },
                        app_version,
                        relay_update,
                    )
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
                protocol: 3,
                code: code_from_peer_or_target.to_string(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                relay_update: true,
            }))
            .await?;
            // 双边同连仲裁：登记过出站尝试时，让行裁决与等 Ack 赛跑——
            // 对方（大 ID 方让行后）连入的链路才是要活的那条，本尝试立刻收尾。
            let my_gen = self.outbound_mark.lock().await.as_ref().map(|(_, g)| *g);
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
                    app_version,
                    relay_update,
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
                    (
                        Device {
                            device_id,
                            name: device_name,
                            code: String::new(),
                            last_seen: now_ms(),
                        },
                        app_version,
                        relay_update,
                    )
                }
                _ => return Err(ServiceError::Other("握手失败：未收到 HelloAck".into())),
            }
        };
        Ok((peer, peer_app_version, peer_relay_update))
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
        let (peer, peer_app_version, peer_relay_update) = tokio::time::timeout(
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
            peer_goodbye: AtomicBool::new(false),
            peer_app_version,
            peer_relay_update,
        });
        // #17：手动/自动连上即解除该对端的重连抑制（抑制只管自动重连）
        self.reconnect_suppress
            .lock()
            .await
            .remove(&session.peer.device_id);
        // 已有旧会话时先按「被新连接替换」收尾再换上新会话，不许无声覆盖
        let old = self.peer.lock().await.clone();
        if let Some(old) = old {
            self.end_session(&old, "被新连接替换", SessionEnd::Replaced)
                .await;
        }
        *self.peer.lock().await = Some(session.clone());
        // 自动重连登记（v2.6.2）：任何新连接都让在跑的重连循环停手；
        // 本次带了拨号路线（出站连接由应用层在 attach 前登记）时刷新
        // 回拨目标；纯入站连接不带路线——目标仍是这位对端则保留旧
        // 路线，换了对端则清掉（没有路线就没法回拨）。
        self.reconnect_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            // 路线只许出站 attach 消费（评审修正）：pending_route 只由
            // 应用层出站拨号路径登记；若那次拨号失败、路线残留，而后
            // 来的是**入站**连接，无条件消费会把入站对端的 ID 配上
            // 别人的路线，重连将拨错设备。入站时只丢弃残留路线。
            let route = if host {
                let _ = self.pending_route.lock().await.take();
                None
            } else {
                self.pending_route.lock().await.take()
            };
            let mut target = self.reconnect_target.lock().await;
            match route {
                Some(route) => {
                    *target = Some(ReconnectTarget {
                        peer_id: session.peer.device_id.clone(),
                        name: session.peer.name.clone(),
                        code: connection_code(&session.peer.device_id),
                        route,
                    });
                }
                None => {
                    if target
                        .as_ref()
                        .map(|t| t.peer_id != session.peer.device_id)
                        .unwrap_or(false)
                    {
                        *target = None;
                    }
                }
            }
        }
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
    pub async fn disconnect(self: &Arc<Self>) {
        let session = self.peer.lock().await.clone();
        match session {
            Some(s) => {
                // #17 告别信号：先尽力把 Goodbye 帧发出去再关链——没有
                // 这一步，对方只见链路结束、一律按意外中断自动重连，
                // 用户手动断 5 次被连回 5 次（2026-11-22 真机定案）。
                // 短超时 best-effort：链路已死时发不出去也不许拖住断开
                // 本身，对方靠抑制窗/冷静期兜底（旧版对端解不出此帧，
                // 按坏帧丢弃，连接照常关闭）。
                let sent = tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    s.conn.send(&Frame::Control(Control::Goodbye)),
                )
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
                log::log_line(
                    "conn",
                    if sent {
                        "已向对方发送主动断开信号（Goodbye），随后关闭连接"
                    } else {
                        "主动断开信号（Goodbye）未能送出（链路可能已断），直接关闭连接"
                    },
                );
                self.end_session(&s, "用户主动断开", SessionEnd::User).await;
            }
            None => {
                // 无会话也要停掉可能在跑的自动重连（重连中点断开）
                self.reconnect_gen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *self.reconnect_target.lock().await = None;
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
    async fn end_session(
        self: &Arc<Self>,
        session: &Arc<PeerSession>,
        reason: &str,
        kind: SessionEnd,
    ) {
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
        // #17 分类兜底：会话期间收到过 Goodbye 的，任何路径（接收循环
        // EOF/心跳判死/发送失败）的 Fault 收尾一律改判「对方主动断
        // 开」——告别帧的语义不许被收尾路径的竞态冲掉。
        let kind = if kind == SessionEnd::Fault && session.peer_goodbye.load(Ordering::Relaxed) {
            log::log_line(
                "conn",
                "本次断开改判：对方此前已发主动断开信号（Goodbye），不触发自动重连",
            );
            SessionEnd::PeerGoodbye
        } else {
            kind
        };
        let reason = if kind == SessionEnd::PeerGoodbye && !reason.contains("对方主动断开") {
            "对方主动断开"
        } else {
            reason
        };
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
        // 自动重连分流（v2.6.2）：用户主动断开清目标、停循环，绝不
        // 重连；意外中断（Fault）且留有回拨目标时拉起重连循环。
        match kind {
            SessionEnd::User | SessionEnd::PeerGoodbye => {
                self.reconnect_gen
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *self.reconnect_target.lock().await = None;
            }
            SessionEnd::Fault => {
                // #17 冷静期记账：120 秒窗内连续 Fault 次数供重连
                // 首次等待递增（告别帧没送达时的断-连循环兜底）
                {
                    let mut hist = self.fault_history.lock().await;
                    let now = std::time::Instant::now();
                    let entry = hist
                        .entry(session.peer.device_id.clone())
                        .or_insert((now, 0));
                    if now.duration_since(entry.0) > std::time::Duration::from_secs(120) {
                        *entry = (now, 0);
                    }
                    // 注：entry.0 是窗口起始时刻，只在重置时更新——
                    // 每拍刷新会让「120 秒窗」变成「距上次 Fault 120
                    // 秒」，偶发 Fault 间隔 <120 秒便永不归零（评审）。
                    entry.1 += 1;
                }
                self.maybe_start_reconnect().await;
            }
            SessionEnd::Replaced => {}
        }
    }

    /// 应用层注册拨号器（见 ReconnectDialer）。未注册则不自动重连。
    pub async fn set_reconnect_dialer(&self, dialer: ReconnectDialer) {
        *self.reconnect_dialer.lock().await = Some(dialer);
    }

    /// 应用层在发起出站连接前登记本次拨号路线（attach 成功时消费）。
    pub async fn set_pending_route(&self, route: String) {
        *self.pending_route.lock().await = Some(route);
    }

    /// 重连代号是否仍有效（拨号器在 attach 前核对）。
    pub fn reconnect_should_continue(&self, gen: u64) -> bool {
        self.reconnect_gen
            .load(std::sync::atomic::Ordering::Relaxed)
            == gen
    }

    /// 意外断线后拉起重连循环（已有循环/无目标/无拨号器时直接返回）。
    async fn maybe_start_reconnect(self: &Arc<Self>) {
        if self.reconnect_active.swap(true, Ordering::Relaxed) {
            return; // 已有循环在跑
        }
        let target = self.reconnect_target.lock().await.clone();
        let has_dialer = self.reconnect_dialer.lock().await.is_some();
        let (Some(target), true) = (target, has_dialer) else {
            self.reconnect_active.store(false, Ordering::Relaxed);
            return;
        };
        // #17 抑制窗：对方发过 Goodbye 且未过期 → 绝不自动重连
        {
            let mut sup = self.reconnect_suppress.lock().await;
            let now = std::time::Instant::now();
            sup.retain(|_, until| *until > now);
            if sup.contains_key(&target.peer_id) {
                log::log_line(
                    "conn",
                    &format!(
                        "自动重连被抑制：{} 此前已发主动断开信号（Goodbye），抑制窗内不许连回",
                        target.name
                    ),
                );
                self.reconnect_active.store(false, Ordering::Relaxed);
                return;
            }
        }
        // #17 冷静期：120 秒内连续 Fault ≥2 次，首次等待从 2 秒起
        // 逐次递增（+5 秒/次、封顶 30 秒）——防告别帧丢失时的撞墙循环
        let extra_first_delay = {
            let hist = self.fault_history.lock().await;
            match hist.get(&target.peer_id) {
                Some((_, n)) if *n >= 2 => (((*n as u64) - 1) * 5).min(30),
                _ => 0,
            }
        };
        let gen = self
            .reconnect_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        log::log_line(
            "conn",
            &format!(
                "连接意外中断，已启动自动重连（目标 {}，退避 2/5/10/30 秒，总限时 180 秒{}）",
                target.name,
                if extra_first_delay > 0 {
                    format!("；冷静期：首次等待额外 +{extra_first_delay} 秒")
                } else {
                    String::new()
                }
            ),
        );
        let node = self.clone();
        tokio::spawn(async move { node.reconnect_loop(target, gen, extra_first_delay).await });
    }

    /// 重连循环：退避 2→5→10→30 秒封顶，总限时 180 秒；代号失效
    /// （用户主动断开/新连接建立）立即停手；成功后发 connected 事件，
    /// 界面据 ConnectionChanged 落回该对端会话（㉛ 落点规则）。
    async fn reconnect_loop(
        self: Arc<Self>,
        target: ReconnectTarget,
        gen: u64,
        extra_first_delay: u64,
    ) {
        const DELAYS: [u64; 4] = [2, 5, 10, 30];
        // 冷静期加时不吃 180 秒重连预算（评审补强）
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(180 + extra_first_delay);
        let mut attempt: u32 = 0;
        let gave_up = loop {
            attempt += 1;
            // #17 冷静期只加在首次等待上，后续退避照旧
            let delay = DELAYS[((attempt - 1) as usize).min(3)]
                + if attempt == 1 { extra_first_delay } else { 0 };
            // 可中断等待：每 200ms 核对一次代号，失效立刻停手
            let until = tokio::time::Instant::now() + std::time::Duration::from_secs(delay);
            while tokio::time::Instant::now() < until {
                if !self.reconnect_should_continue(gen) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            if !self.reconnect_should_continue(gen) || self.is_connected().await {
                break false; // 被取消或已（被别的路径）连上，不算放弃
            }
            if tokio::time::Instant::now() >= deadline {
                break true;
            }
            // #17 补强：抑制窗可能在本循环启动后才登记（对方
            // Goodbye 迟到），每次拨号前复核，命中立即停手。
            {
                let sup = self.reconnect_suppress.lock().await;
                if sup
                    .get(&target.peer_id)
                    .is_some_and(|until| *until > std::time::Instant::now())
                {
                    log::log_line(
                        "conn",
                        &format!(
                            "自动重连停手：{} 已发主动断开信号（Goodbye），抑制窗内不许连回",
                            target.name
                        ),
                    );
                    break false;
                }
            }
            self.emit(Event::ReconnectState {
                peer_name: target.name.clone(),
                attempt,
                state: "trying".into(),
            });
            log::log_line(
                "conn",
                &format!("自动重连：第 {attempt} 次尝试连接 {}", target.name),
            );
            let dialer = self.reconnect_dialer.lock().await.clone();
            let Some(dialer) = dialer else { break false };
            let _ = dialer(self.clone(), target.clone(), gen).await;
            if self.is_connected().await {
                log::log_line(
                    "conn",
                    &format!("自动重连成功（第 {attempt} 次尝试）：{}", target.name),
                );
                self.emit(Event::ReconnectState {
                    peer_name: target.name.clone(),
                    attempt,
                    state: "connected".into(),
                });
                break false;
            }
            if tokio::time::Instant::now() >= deadline {
                break true;
            }
        };
        if gave_up {
            log::log_line(
                "conn",
                &format!(
                    "自动重连停手：180 秒内未能恢复与 {} 的连接（共尝试 {attempt} 次）",
                    target.name
                ),
            );
            self.emit(Event::ReconnectState {
                peer_name: target.name.clone(),
                attempt,
                state: "gave_up".into(),
            });
        }
        self.reconnect_active.store(false, Ordering::Relaxed);
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
                // 静默判定看「成帧活动」与「字节活动」的较新者（#18）：
                // 一张 320KB 剪贴板大图要二十多秒才成帧，只看成帧时刻
                // 会把正在正常传输的连接误判死亡（2026-11-22 双机日志
                // 定案：双向帧流在帧层冻结 19 秒、双边先后判死）。
                let last_frame_ms = session.last_inbound_ms.load(Ordering::Relaxed);
                let last_byte_ms = session.conn.last_inbound_byte_ms();
                let silent_ms = now_ms() - last_frame_ms.max(last_byte_ms) as i64;
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
                            SessionEnd::Fault,
                        )
                        .await;
                        return;
                    }
                }
                if let Err(e) = session.conn.send(&Frame::Control(Control::Ping)).await {
                    node.end_session(
                        &session,
                        &format!("发送失败：心跳帧发送出错（{e}）"),
                        SessionEnd::Fault,
                    )
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
        self.end_session(&session, &reason, SessionEnd::Fault).await;
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
            Frame::Control(Control::Goodbye) => {
                // #17：对方主动断开——给会话打标（收尾时改判、不重连）
                // 并立即登记节点级抑制窗（10 分钟），双保险防竞态漏标
                session.peer_goodbye.store(true, Ordering::Relaxed);
                self.reconnect_suppress.lock().await.insert(
                    session.peer.device_id.clone(),
                    std::time::Instant::now() + std::time::Duration::from_secs(600),
                );
                log::log_line(
                    "conn",
                    &format!(
                        "收到对方主动断开信号（Goodbye）：{}——本次断开后不自动重连",
                        session.peer.name
                    ),
                );
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
                update_pkg,
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
                    update_pkg,
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
                let mut cancel_msg_id = String::new();
                if let Some(st) = self.recv_files.lock().await.remove(&transfer_id) {
                    cancel_msg_id = st.msg_id.clone();
                    let _ = std::fs::remove_file(&st.part_path);
                    let _ = self
                        .storage
                        .update_message_status(&st.msg_id, MessageStatus::Failed);
                    self.emit(Event::MessageStatus {
                        id: st.msg_id.clone(),
                        status: MessageStatus::Failed,
                    });
                }
                self.emit(Event::TransferState {
                    transfer_id,
                    state: "cancelled".into(),
                    msg_id: cancel_msg_id,
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
            Frame::Control(Control::UpdateCheckRequest { req_id, platform }) => {
                // 中继更新·被请求方：代取更新信息后回结果（慢操作，
                // 独立任务跑，不堵接收循环——与 AI 代理同口径）。
                let node = self.clone();
                let session2 = session.clone();
                tokio::spawn(async move {
                    let _ = node.serve_update_check(session2, req_id, platform).await;
                });
            }
            Frame::Control(Control::UpdateCheckResult {
                req_id,
                ok,
                version,
                notes,
                signature,
                error,
            }) => {
                if let Some(tx) = self.update_check_waiters.lock().await.remove(&req_id) {
                    let res = if ok {
                        Ok(RelayUpdateInfo {
                            version: version.unwrap_or_default(),
                            notes: notes.unwrap_or_default(),
                            signature: signature.unwrap_or_default(),
                        })
                    } else {
                        Err(error.unwrap_or_else(|| "对端取更新信息失败".to_string()))
                    };
                    let _ = tx.send(res);
                }
            }
            Frame::Control(Control::UpdateFetchRequest { req_id, platform }) => {
                // 中继更新·被请求方：下载安装包并经文件通道回传，
                // 传完即删本地临时包（不留缓存，用户定案）。
                let node = self.clone();
                let session2 = session.clone();
                tokio::spawn(async move {
                    let _ = node.serve_update_fetch(session2, req_id, platform).await;
                });
            }
            Frame::Control(Control::UpdateFetchResult {
                req_id,
                ok,
                error,
                version,
                signature,
            }) => {
                if let Some(tx) = self.update_fetch_waiters.lock().await.remove(&req_id) {
                    let res = if ok {
                        Ok((version, signature))
                    } else {
                        Err(error.unwrap_or_else(|| "对端下载更新包失败".to_string()))
                    };
                    let _ = tx.send(res);
                }
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
            msg_id: msg_id.to_string(),
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
    /// 锁等待带截止（CLIP_LOCK_BUDGET）：正常持有只有一次读写的毫秒级，
    /// 等不到只可能是某个调用卡在系统调用里（如剪贴板所有者对延迟渲染
    /// 无响应）——等满即放弃返回 None，绝不无限排队；粘贴路径更是不碰
    /// 这把锁（独立短句柄 + 500ms 截止，见 read_clipboard_image_bounded）。
    fn with_clipboard<R>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
        let mut guard = lock_with_deadline(&self.clip_handle, CLIP_LOCK_BUDGET)?;
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
            return Err(ServiceError::Other("图片文件已不存在，无法复制".into()));
        }
        let img = image::open(path)
            .map_err(|_| ServiceError::Other("这张图片解码失败，无法复制".into()))?;
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        // 尺寸上限保护：超大图 RGBA 展开是 4 字节/像素，5000 万像素
        // 已是 200MB 量级，再大直接拒绝并说明，不把应用拖垮。
        if (w as u64) * (h as u64) > 50_000_000 {
            return Err(ServiceError::Other("图片尺寸过大，无法复制到剪贴板".into()));
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

    /// 剪贴板同步触发方式（设置键 clipboard_sync_mode）：auto=复制了
    /// 马上发（现状）；manual=先攒进本机历史缓冲，按快捷键/按钮才发。
    /// 默认 auto；存量值非法时回落 auto。
    pub fn clipboard_sync_mode(&self) -> &'static str {
        match self
            .storage
            .get_setting("clipboard_sync_mode")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("manual") => "manual",
            _ => "auto",
        }
    }

    pub fn set_clipboard_sync_mode(&self, mode: &str) -> Result<(), ServiceError> {
        let mode = if mode == "manual" { "manual" } else { "auto" };
        self.storage.set_setting("clipboard_sync_mode", mode)?;
        log::log_line(
            "clipboard",
            &format!(
                "剪贴板同步方式：{}",
                if mode == "manual" {
                    "手动（按快捷键或按钮才发）"
                } else {
                    "自动（复制了马上发）"
                }
            ),
        );
        Ok(())
    }

    /// 一次同步的条数（设置键 clipboard_sync_count）：只认 1/2/3/5，
    /// 其余一律回落 1（=只发最新一条，与旧行为一致）。
    pub fn clipboard_sync_count(&self) -> usize {
        match self
            .storage
            .get_setting("clipboard_sync_count")
            .ok()
            .flatten()
            .as_deref()
        {
            Some("2") => 2,
            Some("3") => 3,
            Some("5") => 5,
            _ => 1,
        }
    }

    pub fn set_clipboard_sync_count(&self, n: usize) -> Result<(), ServiceError> {
        let n = match n {
            2 | 3 | 5 => n,
            _ => 1,
        };
        self.storage
            .set_setting("clipboard_sync_count", &n.to_string())?;
        log::log_line("clipboard", &format!("剪贴板每次同步条数：最新 {n} 条"));
        Ok(())
    }

    /// 手动同步快捷键（设置键 clipboard_sync_hotkey，显示格式如
    /// "Ctrl+Alt+V"）：默认 Ctrl+Alt+V；空值回落默认。
    pub fn clipboard_sync_hotkey(&self) -> String {
        self.storage
            .get_setting("clipboard_sync_hotkey")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Ctrl+Alt+V".to_string())
    }

    pub fn set_clipboard_sync_hotkey(&self, hotkey: &str) -> Result<(), ServiceError> {
        self.storage.set_setting("clipboard_sync_hotkey", hotkey)?;
        log::log_line("clipboard", &format!("剪贴板手动同步快捷键：{hotkey}"));
        Ok(())
    }

    /// 把一批剪贴板内容按时间正序（旧→新）依次发给对方，返回实际
    /// 发出的条数。逐条走既有口径：超 2MB 跳过并记日志、发送成功
    /// 才登记（note_sent，整批哈希全进发送登记——对方回声抑制的
    /// 第一道闸）。图片不走自动单条路径的出站延后（#18）：批量里
    /// 除最新一条外都是「补发的历史」，再延后只会乱序；手动触发
    /// 更是用户明确指令，不让行。
    async fn send_clip_contents(
        &self,
        session: &Arc<PeerSession>,
        items: Vec<ClipContent>,
    ) -> usize {
        let mut sent = 0usize;
        for content in items {
            let hash = content.hash();
            let ok = match content {
                ClipContent::Text(text) => {
                    if text.len() > CLIPBOARD_MAX_BYTES {
                        log::log_line(
                            "clipboard",
                            &format!("剪贴板文字 {} 字节，超过 2MB 上限，跳过同步", text.len()),
                        );
                        false
                    } else {
                        session
                            .conn
                            .send(&Frame::Control(Control::ClipboardText {
                                hash: hash.clone(),
                                text,
                            }))
                            .await
                            .is_ok()
                    }
                }
                ClipContent::Image {
                    width,
                    height,
                    rgba,
                } => {
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
                        false
                    } else {
                        session
                            .conn
                            .send(&Frame::ClipboardImage(ClipboardImage {
                                hash: digest,
                                width,
                                height,
                                compression: comp_algo,
                                data,
                            }))
                            .await
                            .is_ok()
                    }
                }
            };
            if ok {
                let mut g = self.clip_guard.lock().await;
                g.note_sent(hash);
                sent += 1;
            }
        }
        sent
    }

    /// 手动同步一次（快捷键与设置页按钮共用）：把本机历史缓冲里
    /// 最新 N 条（N=同步条数设置）按序发给对方。未连接/任一侧未开/
    /// 没攒到内容时不发送、只打日志并把原因如实返回给界面提示。
    pub async fn sync_clipboard_now(self: &Arc<Self>) -> ManualSyncOutcome {
        if !self.clipboard_local_on() {
            log::log_line("clipboard", "手动同步未发出：本机剪贴板同步未开启");
            return ManualSyncOutcome::LocalOff;
        }
        let session = match self.session().await {
            Ok(s) => s,
            Err(_) => {
                log::log_line("clipboard", "手动同步未发出：尚未连接设备");
                return ManualSyncOutcome::NotConnected;
            }
        };
        if !self.clipboard_peer_on() {
            log::log_line("clipboard", "手动同步未发出：对方未开启剪贴板同步");
            return ManualSyncOutcome::PeerOff;
        }
        // v1.0.4 #6：触发时先现读一次剪贴板（500ms 截止、读不到不硬
        // 等），读到的新内容以它为准——旧行为只发历史存货，剪贴板被
        // 占用期间复制的内容进不了历史，按快捷键只能发旧货、观感像
        // 没触发。现读被占用则明确 Occupied，绝不静默发旧货装样子；
        // 现读走与监视同一套裁决（poll_decide），回声不回发、无变化
        // 不重复记。读超时/任务失败 = 这次没读到（多半正是被占用卡
        // 住），同样按 Occupied 收尾、绝不回落历史发旧货（第三方评审
        // 采纳：回落发旧货正是 #6 投诉的形态本身）。
        {
            let node = self.clone();
            let fresh = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::task::spawn_blocking(move || node.with_clipboard(clip_read_with)),
            )
            .await;
            if !matches!(fresh, Ok(Ok(_))) {
                log::log_line(
                    "clipboard",
                    "手动同步未发出：现读剪贴板超时（500ms 内没读到，可能被其他程序占用），这次没读到新内容",
                );
                return ManualSyncOutcome::Occupied;
            }
            if let Ok(Ok(Some(read))) = fresh {
                if read.occupied && read.content.is_none() {
                    let owner = clipboard_owner_desc()
                        .map(|o| format!("（占用者：{o}）"))
                        .unwrap_or_default();
                    log::log_line(
                        "clipboard",
                        &format!(
                            "手动同步未发出：现读剪贴板被其他程序占用{owner}，这次没读到新内容"
                        ),
                    );
                    return ManualSyncOutcome::Occupied;
                }
                if let Some(content) = read.content {
                    let seq = read.seq;
                    let mut g = self.clip_guard.lock().await;
                    if matches!(g.poll_decide(&content, seq), ClipPollDecision::Send) {
                        g.note_history(content);
                    }
                }
            }
        }
        let items = {
            let g = self.clip_guard.lock().await;
            g.history_latest(self.clipboard_sync_count())
        };
        if items.is_empty() {
            log::log_line(
                "clipboard",
                "手动同步未发出：还没有攒到可同步的内容（先复制点东西）",
            );
            return ManualSyncOutcome::Empty;
        }
        let sent = self.send_clip_contents(&session, items).await;
        if sent == 0 {
            log::log_line("clipboard", "手动同步未发出：内容超限或发送失败");
            return ManualSyncOutcome::Empty;
        }
        log::log_line("clipboard", &format!("手动同步：已发出最新 {sent} 条"));
        ManualSyncOutcome::Sent(sent)
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
            seq,
            ..
        }) = read
        {
            let mut g = self.clip_guard.lock().await;
            g.local_seen = Some(content.hash());
            g.note_seq(seq);
            // 手动模式例外（用户裁定）：基线内容只登记、绝不自动发，
            // 但要进历史缓冲——用户连上之后按快捷键主动发它，正是
            // 手动模式的意义。自动模式不种：基线之前的旧内容不许
            // 借「多条同步」溜出去（重连不重推旧内容的规矩不破）。
            if self.clipboard_sync_mode() == "manual" {
                g.note_history(content);
            }
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
                let owner = clipboard_owner_desc()
                    .map(|o| format!("（占用者：{o}）"))
                    .unwrap_or_default();
                log::log_line(
                    "clipboard",
                    &format!("读取本机剪贴板失败：剪贴板被其他程序占用{owner}，本轮跳过（不重试）"),
                );
            }
            return;
        }
        let seq = read.seq;
        let Some(content) = read.content else { return };
        let hash = content.hash();
        let decision = {
            let mut g = self.clip_guard.lock().await;
            let d = g.poll_decide(&content, seq);
            if matches!(d, ClipPollDecision::Send) {
                // 本机新复制：先记进历史缓冲（自动多条与手动触发共用，
                // 只存内存、退出即清，绝不落盘）
                g.note_history(content.clone());
            }
            d
        };
        match decision {
            ClipPollDecision::Unchanged => return, // 没变化
            ClipPollDecision::SyncEcho => {
                // 同步写入本机的内容（含系统格式转换后的形态）：
                // 基线已在裁决里推进，绝不回发（#16 防回环核心）
                log::log_verbose("clipboard", "剪贴板监视：读到的是同步写入的内容，不回发");
                return;
            }
            ClipPollDecision::Send => {}
        }
        // 手动模式：只观察不发送——推进基线（不进发送登记，还没发
        // 出去），内容已在历史缓冲里攒着，等快捷键/按钮触发。
        if self.clipboard_sync_mode() == "manual" {
            let mut g = self.clip_guard.lock().await;
            g.note_observed(hash);
            g.note_seq(seq);
            drop(g);
            log::log_verbose(
                "clipboard",
                "剪贴板监视（手动模式）：新内容已攒入待同步，未发送",
            );
            return;
        }
        // 自动模式且条数 >1：最新 N 条按时间正序整批发出。N=1 走下方
        // 既有单条路径，行为与旧版完全一致（含图片出站延后 #18）。
        let count = self.clipboard_sync_count();
        if count > 1 {
            let items = {
                let g = self.clip_guard.lock().await;
                g.history_latest(count)
            };
            let sent = self.send_clip_contents(session, items).await;
            if sent > 0 {
                let mut g = self.clip_guard.lock().await;
                g.note_seq(seq);
                drop(g);
                log::log_line("clipboard", &format!("剪贴板同步：已发送最新 {sent} 条"));
            }
            return;
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
                {
                    let mut g = self.clip_guard.lock().await;
                    g.clip_defer_count = 0;
                    g.clip_defer_hash = None;
                }
                let n = text.len();
                // 发送登记在发送成功后才记（第三方评审采纳）：先登记
                // 再发送的话，发送失败这一份就永远不会重试、静默丢失。
                if session
                    .conn
                    .send(&Frame::Control(Control::ClipboardText {
                        hash: hash.clone(),
                        text,
                    }))
                    .await
                    .is_ok()
                {
                    let mut g = self.clip_guard.lock().await;
                    g.note_sent(hash);
                    g.note_seq(seq);
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
                // 背景优先级（#18）：出站还有大批字节没上空（多半是文件
                // 传输在跑）时，大图先延后，别去和聊天/控制帧抢无线电；
                // 延后期间不登记 last_seen，下一拍内容还在就重试，用户
                // 若已复制新内容则自然换成新内容。连续延后 15 拍（约
                // 10 秒）后照常发送，不许饿死。
                let lag = session.conn.outbound_lag();
                if lag > 64 * 1024 {
                    let mut g = self.clip_guard.lock().await;
                    // 延后的是另一张图（用户刚复制了新图）：计数清零
                    // 重新让行，不继承旧图的延后进度（评审采纳）
                    if g.clip_defer_hash.as_deref() != Some(hash.as_str()) {
                        g.clip_defer_hash = Some(hash.clone());
                        g.clip_defer_count = 0;
                    }
                    if g.clip_defer_count < 15 {
                        g.clip_defer_count += 1;
                        if g.clip_defer_count == 1 {
                            log::log_verbose(
                                "clipboard",
                                "剪贴板图片延后发送：出站队列繁忙（背景优先级）",
                            );
                        }
                        return;
                    }
                }
                {
                    let mut g = self.clip_guard.lock().await;
                    g.clip_defer_count = 0;
                    g.clip_defer_hash = None;
                }
                let n = data.len();
                let frame = Frame::ClipboardImage(ClipboardImage {
                    hash: digest,
                    width,
                    height,
                    compression: comp_algo,
                    data,
                });
                // 发送登记同样在发送成功后才记（评审采纳，同文字路径）
                if session.conn.send(&frame).await.is_ok() {
                    let mut g = self.clip_guard.lock().await;
                    g.note_sent(hash);
                    g.note_seq(seq);
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
        // 永不写空（#8）：空文字/零尺寸图片一律不应用——把空内容写进
        // 剪贴板等于清空用户剪贴板，红线不许。
        match &content {
            ClipContent::Text(t) if t.is_empty() => {
                log::log_verbose("clipboard", "收到对方空文字剪贴板内容，已忽略（永不写空）");
                return;
            }
            ClipContent::Image {
                width,
                height,
                rgba,
            } if *width == 0 || *height == 0 || rgba.is_empty() => {
                log::log_verbose("clipboard", "收到对方空图片剪贴板内容，已忽略（永不写空）");
                return;
            }
            _ => {}
        }
        let hash = content.hash();
        let kind = content.kind();
        let desc = match &content {
            ClipContent::Text(t) => format!("文字（{} 字节）", t.len()),
            ClipContent::Image { width, height, .. } => format!("图片（{width}x{height}）"),
        };
        let last_seen_snapshot;
        {
            let mut g = self.clip_guard.lock().await;
            // #16 状态机裁决：回声（本机发出去过的）直接丢弃不写——
            // 写了会在系统剪贴板历史里把同一内容记第二遍；本机已是
            // 这份内容（或刚应用过等价内容）同样不重复写。
            match g.apply_decide(&content) {
                ClipApplyDecision::EchoDrop => {
                    log::log_line(
                        "clipboard",
                        "收到对方回传的本机内容（回声），已丢弃、不写入本机剪贴板",
                    );
                    return;
                }
                ClipApplyDecision::AlreadyLocal => {
                    g.note_applied(content.clone(), Some(hash.clone()));
                    log::log_verbose(
                        "clipboard",
                        "收到对方剪贴板内容，与本机当前内容相同，跳过写入",
                    );
                    return;
                }
                ClipApplyDecision::Proceed => {}
            }
            // 上次写入失败后的退让期内：直接丢弃，绝不重试硬抢系统剪贴板
            if g.write_backoff_until
                .map(|t| t > std::time::Instant::now())
                .unwrap_or(false)
            {
                log::log_verbose("clipboard", "剪贴板写入退让中，丢弃本条远端内容（不重试）");
                return;
            }
            last_seen_snapshot = g.local_seen.clone();
        }
        // 读→比→写→验在同一个阻塞任务里一次完成（见 clip_apply_atomic）
        let written = content.clone();
        let node = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            node.with_clipboard(|cb| clip_apply_atomic(cb, &written, last_seen_snapshot.as_deref()))
                .unwrap_or((ClipApplyOutcome::Failed("系统剪贴板不可用".into()), 0))
        })
        .await;
        match result {
            Ok((ClipApplyOutcome::Written { readback_hash }, writes)) => {
                let n = {
                    let mut g = self.clip_guard.lock().await;
                    g.write_count += writes;
                    // 应用登记：本体 + 线缆哈希 + 回读哈希一并钉死
                    g.note_applied(content, readback_hash);
                    g.write_count
                };
                log::log_line(
                    "clipboard",
                    &format!("剪贴板同步：已写入对方{desc}（本进程第 {n} 次剪贴板写入）"),
                );
                self.emit(Event::ClipboardSynced {
                    kind: kind.to_string(),
                });
            }
            Ok((ClipApplyOutcome::SkippedSame, _)) => {
                // 本机已是这份内容：只更新应用登记，不重复写
                let mut g = self.clip_guard.lock().await;
                g.note_applied(content, Some(hash));
                log::log_verbose(
                    "clipboard",
                    "收到对方剪贴板内容，本机已是该内容，未重复写入",
                );
            }
            Ok((ClipApplyOutcome::SkippedLocalNewer, _)) => {
                // 本机用户刚复制的新内容优先：不写、不登记应用账——
                // 本机监视下一拍会把用户的新内容同步给对方（最新者胜）。
                log::log_line(
                    "clipboard",
                    "收到对方剪贴板内容，但本机刚被复制了新内容，本机优先、未覆盖",
                );
            }
            Ok((ClipApplyOutcome::Failed(e), writes)) => {
                let mut g = self.clip_guard.lock().await;
                g.write_count += writes;
                drop(g);
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
        let prep = self.prepare_file_send(&session, path).await?;
        let run = self
            .run_file_transfer(
                &session,
                &prep.path,
                prep.name.clone(),
                prep.size,
                prep.sha.clone(),
                prep.transfer_id.clone(),
                prep.msg_id.clone(),
                None,
                false,
            )
            .await?;
        let tid = prep.transfer_id.clone();
        self.finish_file_send(&prep, run).await;
        Ok(tid)
    }

    /// 批量发送文件（v1.0.4 #8 排队环）：先把整批消息全部落库上屏、
    /// 逐个发「排队中」状态（前端据此给每个气泡挂灰色等待环），然后
    /// 按序逐个真传——与旧前端逐个 invoke 的语义一致：单个失败记入
    /// 返回的说明列表、不堵后面。返回逐个的失败说明（空 = 全部发出）。
    pub async fn send_files_batch(&self, paths: &[PathBuf]) -> Vec<String> {
        let mut errors = Vec::new();
        let session = match self.session().await {
            Ok(s) => s,
            Err(e) => {
                return paths
                    .iter()
                    .map(|p| {
                        format!(
                            "文件「{}」发送失败：{e}",
                            p.file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.to_string_lossy().to_string())
                        )
                    })
                    .collect();
            }
        };
        let mut prepared: Vec<PreparedFileSend> = Vec::with_capacity(paths.len());
        for p in paths {
            match self.prepare_file_send(&session, p).await {
                Ok(prep) => {
                    self.emit(Event::TransferState {
                        transfer_id: prep.transfer_id.clone(),
                        state: "queued".into(),
                        msg_id: prep.msg_id.clone(),
                    });
                    prepared.push(prep);
                }
                Err(e) => errors.push(format!(
                    "文件「{}」发送失败：{e}",
                    p.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.to_string_lossy().to_string())
                )),
            }
        }
        for prep in &prepared {
            // 排队期间被用户取消：不发要约、直接按取消收尾
            if self.take_cancelled(&prep.transfer_id).await {
                self.finish_cancelled(&prep.transfer_id, &prep.msg_id).await;
                continue;
            }
            match self
                .run_file_transfer(
                    &session,
                    &prep.path,
                    prep.name.clone(),
                    prep.size,
                    prep.sha.clone(),
                    prep.transfer_id.clone(),
                    prep.msg_id.clone(),
                    None,
                    false,
                )
                .await
            {
                Ok(run) => self.finish_file_send(prep, run).await,
                Err(e) => {
                    // 传输主体出错（如会话中断）：消息标失败并清掉
                    // 气泡上的环，失败记入说明；后面的照常逐个尝试
                    let _ = self
                        .storage
                        .update_message_status(&prep.msg_id, MessageStatus::Failed);
                    self.emit(Event::MessageStatus {
                        id: prep.msg_id.clone(),
                        status: MessageStatus::Failed,
                    });
                    self.emit(Event::TransferState {
                        transfer_id: prep.transfer_id.clone(),
                        state: "cancelled".into(),
                        msg_id: prep.msg_id.clone(),
                    });
                    errors.push(format!("文件「{}」发送失败：{e}", prep.name));
                }
            }
        }
        errors
    }

    /// 备好一个待发文件：读元数据、算哈希、落文件记录与消息并发
    /// MessageNew。send_file 与 send_files_batch 共用。
    async fn prepare_file_send(
        &self,
        session: &Arc<PeerSession>,
        path: &Path,
    ) -> Result<PreparedFileSend, ServiceError> {
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
        Ok(PreparedFileSend {
            msg_id: msg.id,
            name,
            size,
            sha,
            transfer_id,
            path: path.to_path_buf(),
        })
    }

    /// 一次文件发送跑完后按结果收尾（完成日志、消息状态、状态事件）：
    /// send_file 与批量发送共用，口径与旧单发完全一致。Cancelled 时
    /// run_file_transfer 内部已走 finish_cancelled，这里不重复处理。
    async fn finish_file_send(&self, prep: &PreparedFileSend, run: TransferRun) {
        let (name, size, transfer_id, msg_id) =
            (&prep.name, prep.size, &prep.transfer_id, &prep.msg_id);
        match run {
            TransferRun::Cancelled => {}
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
                            if used_compression { COMPRESSION_LABEL } else { "无" }
                        ),
                    );
                    let _ = self
                        .storage
                        .update_message_status(msg_id, MessageStatus::Delivered);
                    // 状态事件补发：文件消息没有 Delivered 控制帧回执，
                    // 不发事件前端气泡会一直停在「发送中」（v2.5.2 前的老缺口）
                    self.emit(Event::MessageStatus {
                        id: msg_id.clone(),
                        status: MessageStatus::Delivered,
                    });
                    self.emit(Event::TransferState {
                        transfer_id: transfer_id.clone(),
                        state: "done".into(),
                        msg_id: msg_id.clone(),
                    });
                } else {
                    let _ = self
                        .storage
                        .update_message_status(msg_id, MessageStatus::Failed);
                    self.emit(Event::MessageStatus {
                        id: msg_id.clone(),
                        status: MessageStatus::Failed,
                    });
                    self.emit(Event::TransferState {
                        transfer_id: transfer_id.clone(),
                        state: "verify_failed".into(),
                        msg_id: msg_id.clone(),
                    });
                }
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
        update_pkg: bool,
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
                update_pkg,
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
        // v1.0.4 验后 #1：发送进度改报「已上空」估算，不再按已入队字节。
        // conn.send 返回只代表帧进了出站队列（8 帧深、帧可达 256KB，
        // 慢链路上队列里能堵住近四成文件），旧口径发送端虚高、接收端
        // 按落盘真值走，两边对不上（真机实测发送 34% vs 接收 4%）。
        // 新口径与排空段同一公式：进度 = 已发文件字节 × (1 − 出站滞留
        // / 已发空中字节），由 120ms ticker 在发送全程持续发；ticker
        // 与排空段共用 prog_last 做单调钳制，数字只许前进不许回退。
        let covered0 = covered_bytes(&received);
        let prog_sent = Arc::new(AtomicU64::new(sent_total));
        let prog_wire = Arc::new(AtomicU64::new(0));
        let prog_stop = Arc::new(AtomicBool::new(false));
        let prog_last = Arc::new(AtomicU64::new(covered0));
        struct ProgStopGuard(Arc<AtomicBool>);
        impl Drop for ProgStopGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _prog_guard = ProgStopGuard(prog_stop.clone());
        {
            let events = self.events.clone();
            let conn = session.conn.clone();
            let sent_a = prog_sent.clone();
            let wire_a = prog_wire.clone();
            let stop_a = prog_stop.clone();
            let last_a = prog_last.clone();
            let tid = transfer_id.clone();
            let mid = msg_id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                    if stop_a.load(Ordering::Relaxed) {
                        break;
                    }
                    let wire = wire_a.load(Ordering::Relaxed);
                    if wire == 0 {
                        continue;
                    }
                    let sent = sent_a.load(Ordering::Relaxed);
                    let lag = conn.outbound_lag();
                    let frac = 1.0 - (lag as f64 / (wire + 256) as f64).min(1.0);
                    let est = (((sent as f64) * frac) as u64).max(covered0).min(sent);
                    // 发射前再查一次停标记：主流程若在本次计算期间已
                    // 出错返回（断线等），不许抢在失败状态前补一发
                    // 按最后成功值算出的假进度（第三方评审采纳）
                    if stop_a.load(Ordering::Relaxed) {
                        break;
                    }
                    if est > last_a.fetch_max(est, Ordering::Relaxed) {
                        let _ = events.send(Event::TransferProgress {
                            transfer_id: tid.clone(),
                            msg_id: mid.clone(),
                            received: est,
                            total: size,
                        });
                    }
                }
            });
        }
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
                // 进度不再在这里按已入队字节直发：ticker 按已上空
                // 估算持续发（见发送段开头的 #1 口径），这里只喂数。
                prog_sent.store(sent_total, Ordering::Relaxed);
                prog_wire.store(wire_bytes, Ordering::Relaxed);
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
        // #12 状态语义（v2.6.5，日志定案）：conn.send 返回只代表字节进了
        // 出站队列，BLE 上最后一片的空中尾巴能拖二十多秒——旧版在这里
        // 立刻报「校验中」，把整段发送尾巴记在校验名下（真机实测 277KB
        // 文件 22.4 秒里大半是这种假校验）。从此先等出站真正排空（泵把
        // 字节全部交给无线电，outbound_lag 归零），排空期间持续发真实
        // 进度；排空后才许进「校验中」。
        {
            let wire_total = (wire_bytes + 256) as f64; // 加控制帧的大致开销
            let drain_start = std::time::Instant::now();
            let mut drained = false;
            loop {
                let lag = session.conn.outbound_lag();
                if lag == 0 {
                    drained = true;
                    break;
                }
                // 会话已断开/被替换：别干等，交给下面的校验等待收尾报错
                {
                    let guard = self.peer.lock().await;
                    let alive = matches!(&*guard, Some(cur) if Arc::ptr_eq(cur, session));
                    if !alive {
                        break;
                    }
                }
                if drain_start.elapsed() > std::time::Duration::from_secs(120) {
                    log::log_line(
                        "files",
                        &format!("发送排空等待超时（{name}，仍滞留 {lag} 字节），按现状进入等校验"),
                    );
                    break;
                }
                let frac = 1.0 - (lag as f64 / wire_total).min(1.0);
                let aired_est = ((sent_total as f64) * frac) as u64;
                let aired_est = aired_est.max(covered_bytes(&received));
                // 与发送段 ticker 共用单调钳制：排空估算只许前进
                if aired_est > prog_last.fetch_max(aired_est, Ordering::Relaxed) {
                    self.emit(Event::TransferProgress {
                        transfer_id: transfer_id.clone(),
                        msg_id: msg_id.clone(),
                        received: aired_est,
                        total: size,
                    });
                }
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            }
            // 排空完成：进度如实落到已发总量（随后进「校验中」）；
            // 会话死亡/超时跳出的不发满格假进度，交给校验等待收尾报错
            if drained {
                prog_last.store(sent_total, Ordering::Relaxed);
                self.emit(Event::TransferProgress {
                    transfer_id: transfer_id.clone(),
                    msg_id: msg_id.clone(),
                    received: sent_total,
                    total: size,
                });
            }
        }
        // 出站已真正排空：这才轮到对方校验，状态如实切「校验中」。
        self.emit(Event::TransferState {
            transfer_id: transfer_id.clone(),
            state: "verifying".into(),
            msg_id: msg_id.clone(),
        });
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
                    false,
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
        update_pkg: bool,
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
                update_pkg,
            },
        );
        // 更新包不进文件库、不建消息：只走传输账与专用落地（on_file_done）。
        if !update_pkg {
            self.storage.save_file(&FileMeta {
                id: file_id.clone(),
                name: name.clone(),
                size,
                sha256: sha256.clone(),
                path: String::new(),
                mime: None,
            })?;
        }
        match &group {
            // 更新包：无组、无消息，收齐后由 on_file_done 发专用事件
            _ if update_pkg => {}
            // 图片组成员：不单独建消息行，并入组消息清单（首个成员建组）
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
        session: &Arc<PeerSession>,
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
            match decompress(&chunk.data, chunk.compression) {
                Ok(d) => d,
                Err(e) => {
                    // 故障隔离（v2.6.2 硬规）：单片解压失败只许停在这
                    // 一条文件消息上——立刻收尾回 FileVerify{ok:false}
                    // 让发送端把该条标失败，连接与后续消息全程不受
                    // 影响。旧版在这里 `?` 冒泡后发送端永远等不到
                    // 确认、传输挂死（审计表 #F3）。
                    let tid = chunk.transfer_id.clone();
                    let bad = guard.remove(&tid);
                    drop(guard);
                    if let Some(bad) = bad {
                        let _ = std::fs::remove_file(&bad.part_path);
                        self.emit(Event::TransferState {
                            transfer_id: tid.clone(),
                            state: "verify_failed".into(),
                            msg_id: bad.msg_id.clone(),
                        });
                        log::log_line(
                            "files",
                            &format!(
                                "接收分片解压失败（{}），该文件已判失败、连接保持：{e}",
                                bad.name
                            ),
                        );
                    }
                    let _ = session
                        .conn
                        .send(&Frame::Control(Control::FileVerify {
                            transfer_id: tid,
                            ok: false,
                        }))
                        .await;
                    return Ok(());
                }
            }
        };
        // ⑳ .part 句柄常驻（首片打开、整程复用）：逐片 open/close 在
        // 快速链路上是纯浪费，还拖慢接收队列排空、让对方等校验更久。
        // 写盘 IO 失败与解压失败同处置（故障隔离）：该文件判失败并
        // 回执，连接保持——绝不让发送端干等确认（评审+审计补强）。
        if st.file.is_none() {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&st.part_path)
            {
                Ok(h) => st.file = Some(h),
                Err(e) => {
                    let tid = chunk.transfer_id.clone();
                    let bad = guard.remove(&tid);
                    drop(guard);
                    if let Some(bad) = bad {
                        let _ = std::fs::remove_file(&bad.part_path);
                        self.emit(Event::TransferState {
                            transfer_id: tid.clone(),
                            state: "verify_failed".into(),
                            msg_id: bad.msg_id.clone(),
                        });
                        log::log_line(
                            "files",
                            &format!(
                                "接收打开半成品失败（{}），该文件已判失败、连接保持：{e}",
                                bad.name
                            ),
                        );
                    }
                    let _ = session
                        .conn
                        .send(&Frame::Control(Control::FileVerify {
                            transfer_id: tid,
                            ok: false,
                        }))
                        .await;
                    return Ok(());
                }
            }
        }
        let f = st.file.as_mut().unwrap();
        let io_res = f
            .seek(SeekFrom::Start(chunk.offset))
            .and_then(|_| f.write_all(&data));
        if let Err(e) = io_res {
            let tid = chunk.transfer_id.clone();
            let bad = guard.remove(&tid);
            drop(guard);
            if let Some(bad) = bad {
                let _ = std::fs::remove_file(&bad.part_path);
                self.emit(Event::TransferState {
                    transfer_id: tid.clone(),
                    state: "verify_failed".into(),
                    msg_id: bad.msg_id.clone(),
                });
                log::log_line(
                    "files",
                    &format!(
                        "接收写盘失败（{}），该文件已判失败、连接保持：{e}",
                        bad.name
                    ),
                );
            }
            let _ = session
                .conn
                .send(&Frame::Control(Control::FileVerify {
                    transfer_id: tid,
                    ok: false,
                }))
                .await;
            return Ok(());
        }
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
        let mid = self
            .recv_files
            .lock()
            .await
            .get(&transfer_id)
            .map(|st| st.msg_id.clone())
            .unwrap_or_default();
        self.emit(Event::TransferState {
            transfer_id: transfer_id.clone(),
            state: "verifying".into(),
            msg_id: mid,
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
        // 摘要计算失败（读盘/任务异常）与校验不符同处置：判该文件
        // 失败、照常回执，绝不让发送端干等（故障隔离，审计补强）。
        let digest_res: Result<(String, &'static str), String> =
            if st.hasher.is_some() && st.hashed_upto == st.total {
                let digest = format!("{:x}", st.hasher.take().unwrap().finalize());
                Ok((digest, "增量哈希"))
            } else {
                // 整文件重算挪出接收循环：大文件要读数秒，堵在循环里
                // 会让心跳与其它帧都读不到（v2.5.2 实测撞出：校验期
                // 会话被自家心跳误判死亡）。spawn_blocking 里算。
                let part = st.part_path.clone();
                match tokio::task::spawn_blocking(move || sha256_file(&part)).await {
                    Ok(Ok(d)) => Ok((d, "整文件重算")),
                    Ok(Err(e)) => Err(format!("读取半成品算摘要失败：{e}")),
                    Err(e) => Err(format!("校验任务失败：{e}")),
                }
            };
        let (actual, verify_how) = match digest_res {
            Ok(v) => v,
            Err(why) => {
                drop(st.file.take());
                let _ = std::fs::remove_file(&st.part_path);
                self.emit(Event::TransferState {
                    transfer_id: transfer_id.clone(),
                    state: "verify_failed".into(),
                    msg_id: st.msg_id.clone(),
                });
                log::log_line(
                    "files",
                    &format!(
                        "接收收尾失败（{}），该文件已判失败、连接保持：{why}",
                        st.name
                    ),
                );
                let _ = session
                    .conn
                    .send(&Frame::Control(Control::FileVerify {
                        transfer_id,
                        ok: false,
                    }))
                    .await;
                return Ok(());
            }
        };
        let verify_ms = verify_t0.elapsed().as_secs_f64() * 1000.0;
        let ok = actual == st.sha256;
        // 先放掉常驻 .part 句柄再 rename：Windows 上句柄开着改名靠
        // 共享标志兜底，显式释放把这条风险摘掉（flush 已在写片时完成）。
        drop(st.file.take());
        // #13 按日分夹：落地进接收目录下以接收日期命名的子文件夹
        // （自动创建）；半成品 .part 仍留在接收目录根下不挪窝。
        // 中继更新包例外：落更新暂存目录、文件名只取末段（防对端给的
        // 名字带路径穿越），且不走消息/文件库回写，只把传输账收尾。
        let (land_dir, final_path) = if st.update_pkg {
            let fname = Path::new(&st.name)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "update-package.bin".to_string());
            let dir = self.updates_dir.clone();
            let p = dir.join(fname);
            (dir, p)
        } else {
            let dir = self.download_dir().join(receive_date_dir_name(now_ms()));
            let p = dir.join(&st.name);
            (dir, p)
        };
        // rename/落库失败同判该文件失败（不冒泡挂死发送端）。
        let mut ok = ok;
        if ok {
            let landed = std::fs::create_dir_all(&land_dir)
                .and_then(|_| std::fs::rename(&st.part_path, &final_path))
                .and_then(|_| {
                    if st.update_pkg {
                        // 更新包：传输账标完成即可（无消息/文件记录可回写）
                        self.storage
                            .save_transfer(
                                &transfer_id,
                                "",
                                "in",
                                &session.peer.device_id,
                                st.total,
                                &vec![(0, st.total)],
                                "done",
                            )
                            .map_err(std::io::Error::other)
                    } else {
                        // ⑳ 三笔回写（消息状态/落地路径/传输收尾）合并成一个
                        // 事务，只剩一次提交落盘——文件菜单靠落地路径定位。
                        self.storage
                            .finish_received_file(
                                &st.msg_id,
                                &st.file_id,
                                &final_path.to_string_lossy(),
                                &transfer_id,
                                &session.peer.device_id,
                                st.total,
                                &st.ranges,
                            )
                            .map_err(std::io::Error::other)
                    }
                });
            if let Err(e) = landed {
                log::log_line(
                    "files",
                    &format!("接收落盘失败（{}），该文件已判失败、连接保持：{e}", st.name),
                );
                ok = false;
            }
        }
        if ok {
            self.emit(Event::TransferState {
                transfer_id: transfer_id.clone(),
                state: "done".into(),
                msg_id: st.msg_id.clone(),
            });
            if st.update_pkg {
                // 更新包就位：应用层接手验签与安装确认（签名未验，
                // 事件注释已写明，应用层绝不可跳过验签直接装）。
                self.emit(Event::UpdatePackageReady {
                    transfer_id: transfer_id.clone(),
                    path: final_path.to_string_lossy().to_string(),
                    name: st.name.clone(),
                    size: st.total,
                });
            }
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
                msg_id: st.msg_id.clone(),
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

    // ---------- 中继更新（内网机经外网机代下安装包） ----------

    /// 中继代取/代下用的 HTTP 客户端：若环境里设了 HTTPS_PROXY 则
    /// 显式沿用（部分网络环境只能经代理出网，reqwest 默认构建不读
    /// 环境变量）；没设则直连，与普通机器上的行为一致。根证书除
    /// 内置集外再并入系统证书库——经企业代理出网时对端证书常由
    /// 企业 CA 签发，只认内置集会误判连不上。
    fn update_http_client() -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .tls_built_in_native_certs(true)
            // 总超时与取包等待者（十分钟）对齐，防慢速连接无限挂起
            .timeout(std::time::Duration::from_secs(600))
            .connect_timeout(std::time::Duration::from_secs(30));
        if let Ok(p) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
            if let Ok(proxy) = reqwest::Proxy::all(&p) {
                builder = builder.proxy(proxy);
            }
        }
        builder.build().unwrap_or_else(|_| reqwest::Client::new())
    }

    /// 代取最新版更新信息：拉更新端点的 latest.json，按平台键取出
    /// 版本号、说明、下载地址与签名。端点与应用内更新（updater 插件）
    /// 同一地址——GitHub 最新正式版，草稿不会被推送到这里。
    async fn fetch_latest_update(platform: &str) -> Result<(RelayUpdateInfo, String), String> {
        const ENDPOINT: &str =
            "https://github.com/123zpc/BlueLink-Refactor/releases/latest/download/latest.json";
        let client = Self::update_http_client();
        let resp = client
            .get(ENDPOINT)
            .send()
            .await
            .map_err(|e| format!("连不上更新服务器：{e}"))?;
        if !resp.status().is_success() {
            return Err(format!("更新服务器返回 {}", resp.status()));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("更新信息解析失败：{e}"))?;
        let version = v
            .get("version")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if version.is_empty() {
            return Err("更新信息缺少版本号".into());
        }
        let notes = v
            .get("notes")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let entry = v
            .get("platforms")
            .and_then(|p| p.get(platform))
            .ok_or_else(|| format!("更新信息里没有本平台（{platform}）的安装包"))?;
        let dl = entry
            .get("url")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let signature = entry
            .get("signature")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if dl.is_empty() || signature.is_empty() {
            return Err("更新信息不完整（缺下载地址或签名）".into());
        }
        // 下载地址只认官方仓库域名：更新信息虽经 HTTPS 取回，多一道
        // 前缀校验防将来端点/内容被篡改时把下载引去别处（终局还有验签）
        if !dl.starts_with("https://github.com/") {
            return Err("更新信息里的下载地址不是官方地址，已拒绝".into());
        }
        Ok((
            RelayUpdateInfo {
                version,
                notes,
                signature,
            },
            dl,
        ))
    }

    /// 把最新版安装包下载到更新暂存目录，返回本地路径与本次取包所据
    /// 的更新信息（文件名取下载地址末段）。整体读入内存再落盘：安装
    /// 包十几 MB 可接受，换来失败时不留半截文件；落盘先写临时名再
    /// 改名，防半写文件被当完整包。大小上限 200 MB：官方包远小于此，
    /// 超限视为异常来源、直接拒绝（防异常响应打爆内存/磁盘）。
    async fn download_update_package(
        &self,
        platform: &str,
        req_id: &str,
    ) -> Result<(PathBuf, RelayUpdateInfo), String> {
        const MAX_PKG_BYTES: u64 = 200 * 1024 * 1024;
        let (info, dl) = Self::fetch_latest_update(platform).await?;
        let fname = dl
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("update-package.bin")
            .to_string();
        let client = Self::update_http_client();
        let resp = client
            .get(&dl)
            .send()
            .await
            .map_err(|e| format!("下载更新包失败：{e}"))?;
        if !resp.status().is_success() {
            return Err(format!("下载更新包失败：服务器返回 {}", resp.status()));
        }
        if let Some(n) = resp.content_length() {
            if n > MAX_PKG_BYTES {
                return Err("更新包大小异常（超过 200 MB），已拒绝下载".into());
            }
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("下载更新包中断：{e}"))?;
        if bytes.len() as u64 > MAX_PKG_BYTES {
            return Err("更新包大小异常（超过 200 MB），已拒绝".into());
        }
        std::fs::create_dir_all(&self.updates_dir)
            .map_err(|e| format!("建更新暂存目录失败：{e}"))?;
        let path = self.updates_dir.join(&fname);
        let tmp = self.updates_dir.join(format!(".relay-{req_id}.tmp"));
        std::fs::write(&tmp, &bytes).map_err(|e| format!("写更新包失败：{e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("落更新包失败：{e}"))?;
        Ok((path, info))
    }

    /// 中继更新·被请求方处理查版本：代取最新版信息后回给请求方。
    async fn serve_update_check(
        &self,
        session: Arc<PeerSession>,
        req_id: String,
        platform: String,
    ) -> Result<(), ServiceError> {
        let (ok, version, notes, signature, error) =
            match Self::fetch_latest_update(&platform).await {
                Ok((info, _url)) => (
                    true,
                    Some(info.version),
                    Some(info.notes),
                    Some(info.signature),
                    None,
                ),
                Err(e) => (false, None, None, None, Some(e)),
            };
        session
            .conn
            .send(&Frame::Control(Control::UpdateCheckResult {
                req_id,
                ok,
                version,
                notes,
                signature,
                error,
            }))
            .await?;
        Ok(())
    }

    /// 中继更新·被请求方处理取包：下载安装包 → 回「已就绪」→ 经文件
    /// 通道（update_pkg 标记）发给请求方 → 无论成败删本地临时包
    /// （外网机不留缓存，用户定案）。包本体的到达与完整性由文件传输
    /// 自身的进度/校验事件负责，本函数只管下载阶段的成败回执。
    async fn serve_update_fetch(
        &self,
        session: Arc<PeerSession>,
        req_id: String,
        platform: String,
    ) -> Result<(), ServiceError> {
        let (tmp, info) = match self.download_update_package(&platform, &req_id).await {
            Ok(v) => v,
            Err(e) => {
                log::log_line("update", &format!("中继代下更新包失败：{e}"));
                let _ = session
                    .conn
                    .send(&Frame::Control(Control::UpdateFetchResult {
                        req_id,
                        ok: false,
                        error: Some(e),
                        version: None,
                        signature: None,
                    }))
                    .await;
                return Ok(());
            }
        };
        log::log_line(
            "update",
            &format!("中继代下更新包完成：{}，开始经蓝牙回传", tmp.display()),
        );
        if let Err(e) = session
            .conn
            .send(&Frame::Control(Control::UpdateFetchResult {
                req_id: req_id.clone(),
                ok: true,
                error: None,
                // 与下载同一次快照的版本与签名随应答回传，请求方据此
                // 核对所示版本、并以这份签名验包（评审补强：防两次
                // 请求间发布变化导致签名与包错位）
                version: Some(info.version.clone()),
                signature: Some(info.signature.clone()),
            }))
            .await
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(ServiceError::Other(format!("回取包应答失败：{e}")));
        }
        let name = tmp
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let size = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let sha = sha256_file(&tmp)?;
        let transfer_id = derive_transfer_id(&sha, &session.peer.device_id);
        let msg_id = format!("update:{req_id}");
        let run = self
            .run_file_transfer(
                &session,
                &tmp,
                name,
                size,
                sha,
                transfer_id,
                msg_id,
                None,
                true,
            )
            .await;
        // 传完即删：成功、失败、被取消都删，不在本机留缓存包
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = run {
            log::log_line("update", &format!("中继回传更新包失败：{e}"));
        }
        Ok(())
    }

    /// 当前对端的中继更新能力：（对端名、对端版本、是否支持中继）。
    /// 未连接时返回 None。应用层据此做检查更新的分流提示。
    pub async fn peer_update_caps(&self) -> Option<(String, String, bool)> {
        let session = self.peer.lock().await.clone()?;
        Some((
            session.peer.name.clone(),
            session.peer_app_version.clone(),
            session.peer_relay_update,
        ))
    }

    /// 中继更新·请求方查版本：请当前对端代取最新版信息。
    pub async fn relay_update_check(
        &self,
        platform: &str,
    ) -> Result<RelayUpdateInfo, ServiceError> {
        let session = self.session().await?;
        let req_id = new_id();
        let (tx, rx) = oneshot::channel();
        self.update_check_waiters
            .lock()
            .await
            .insert(req_id.clone(), tx);
        if let Err(e) = session
            .conn
            .send(&Frame::Control(Control::UpdateCheckRequest {
                req_id: req_id.clone(),
                platform: platform.to_string(),
            }))
            .await
        {
            self.update_check_waiters.lock().await.remove(&req_id);
            return Err(ServiceError::Other(format!("发送中继查版本请求失败：{e}")));
        }
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(Ok(info))) => Ok(info),
            Ok(Ok(Err(msg))) => Err(ServiceError::Other(msg)),
            Ok(Err(_)) => {
                self.update_check_waiters.lock().await.remove(&req_id);
                Err(ServiceError::Other("对端连接已断开".into()))
            }
            Err(_) => {
                self.update_check_waiters.lock().await.remove(&req_id);
                Err(ServiceError::Other("对端取更新信息超时".into()))
            }
        }
    }

    /// 中继更新·请求方取包：请对端下载安装包并经文件通道回传。
    /// 返回（req_id, 对端本次取包实际所据的版本, 签名）；包的进度走
    /// 传输事件（msg_id 为 update:{req_id}），到达走
    /// UpdatePackageReady 事件，应用层据此推进更新框。
    pub async fn relay_update_fetch(
        &self,
        platform: &str,
    ) -> Result<(String, Option<String>, Option<String>), ServiceError> {
        let session = self.session().await?;
        let req_id = new_id();
        let (tx, rx) = oneshot::channel();
        self.update_fetch_waiters
            .lock()
            .await
            .insert(req_id.clone(), tx);
        if let Err(e) = session
            .conn
            .send(&Frame::Control(Control::UpdateFetchRequest {
                req_id: req_id.clone(),
                platform: platform.to_string(),
            }))
            .await
        {
            self.update_fetch_waiters.lock().await.remove(&req_id);
            return Err(ServiceError::Other(format!("发送中继取包请求失败：{e}")));
        }
        // 对端下载十几 MB 需要时间：给足十分钟，超时按失败处理
        match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
            Ok(Ok(Ok((version, signature)))) => Ok((req_id, version, signature)),
            Ok(Ok(Err(msg))) => Err(ServiceError::Other(msg)),
            Ok(Err(_)) => {
                self.update_fetch_waiters.lock().await.remove(&req_id);
                Err(ServiceError::Other("对端连接已断开".into()))
            }
            Err(_) => {
                self.update_fetch_waiters.lock().await.remove(&req_id);
                Err(ServiceError::Other("对端下载更新包超时".into()))
            }
        }
    }

    /// 更新暂存目录（应用层校验待装包路径、清理无主包时用）。
    pub fn updates_dir(&self) -> PathBuf {
        self.updates_dir.clone()
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
                if md.is_dir() {
                    // #13 按日分夹后，落地文件在日期子目录里，统计要跟下去
                    if let Ok(sub) = std::fs::read_dir(e.path()) {
                        for se in sub.flatten() {
                            let Ok(smd) = se.metadata() else { continue };
                            if smd.is_file() {
                                files_bytes += smd.len();
                                files_count += 1;
                            }
                        }
                    }
                    continue;
                }
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

    /// 按范围清空聊天记录（v2.6.5，#14；v1.0.2 ④ 加删文件选项）：range
    /// = "24h" | "7d" | "30d" | "all"。只清范围内的消息，范围外不动。
    /// delete_files=true 时，先按托管边界删磁盘文件、再清消息记录：
    /// 只删范围内消息关联的、位于应用托管边界内的实体文件——数据目录
    /// （默认下载目录的父目录，含 downloads/ 与 ai-att/）或当前下载
    /// 目录内；数据目录外的用户自有文件绝不碰（与改名功能的托管边界
    /// 同口径，并多认当前下载目录——用户改过保存目录后收到的文件也
    /// 在托管内）。同一份文件记录若还被范围外的消息引用，跳过不删。
    /// 返回（删除消息条数，删除文件个数）。
    pub fn clear_chat_history_range(
        &self,
        range: &str,
        delete_files: bool,
    ) -> Result<(u64, u64), ServiceError> {
        let now = now_ms();
        let cutoff = match range {
            "all" => None,
            "24h" => Some(now - 24 * 3600 * 1000),
            "7d" => Some(now - 7 * 24 * 3600 * 1000),
            "30d" => Some(now - 30 * 24 * 3600 * 1000),
            other => return Err(ServiceError::Other(format!("未知的清理范围：{other}"))),
        };
        let deleted_files = if delete_files {
            self.delete_files_in_range(cutoff)?
        } else {
            0
        };
        let n = self.storage.clear_messages_range(cutoff)?;
        log::log_line(
            "storage",
            &format!(
                "已按范围清空聊天记录（范围 {range}，共删除 {n} 条消息，相关文件 {deleted_files} 个）"
            ),
        );
        Ok((n, deleted_files))
    }

    /// 托管边界（v1.0.2 ④）：数据目录与当前下载目录，规范化后去重。
    /// 收到文件、AI 附件副本、暂存文档都在数据目录内；用户改过保存
    /// 目录后，当前下载目录也可能在数据目录外，两处都认。
    fn managed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.download_dir()];
        // 数据目录（默认下载目录的父目录）也认；但父目录若是文件系统根
        // （如 "/" 或盘符根），starts_with 恒真等于无边界，直接跳过
        // （第三方评审：default_download_dir 为 "/x" 时 parent 是 "/"）。
        if let Some(parent) = self
            .default_download_dir
            .parent()
            .filter(|p| p.parent().is_some())
        {
            roots.push(parent.to_path_buf());
        }
        let mut canon: Vec<PathBuf> = roots
            .into_iter()
            .map(|p| {
                std::fs::canonicalize(&p).unwrap_or_else(|_| {
                    // 目录不存在时按词法规范化（去掉 . 与多余分隔符）
                    let mut out = PathBuf::new();
                    for c in p.components() {
                        use std::path::Component;
                        match c {
                            Component::CurDir => {}
                            Component::ParentDir => {
                                out.pop();
                            }
                            other => out.push(other.as_os_str()),
                        }
                    }
                    out
                })
            })
            .collect();
        canon.sort();
        canon.dedup();
        canon
    }

    /// 路径是否在托管边界内（v1.0.2 ④）：存在的文件规范化后比对；
    /// 已不在磁盘的按词法绝对路径比对（记录路径失效时仍按边界判定）。
    fn path_is_managed(&self, path: &str, roots: &[PathBuf]) -> bool {
        let p = PathBuf::from(path);
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        let norm = std::fs::canonicalize(&abs).unwrap_or(abs);
        roots.iter().any(|r| norm.starts_with(r))
    }

    /// 删范围内的关联文件（v1.0.2 ④）：先收集候选、逐项过托管边界与
    /// 范围外引用检查，磁盘删完再回删记录；边界外、被范围外消息引用、
    /// 磁盘删除失败的一律跳过（只记日志，不让单文件失败拖垮整次清理）。
    /// 返回实际删除的文件个数。
    fn delete_files_in_range(&self, cutoff_ms: Option<i64>) -> Result<u64, ServiceError> {
        let roots = self.managed_roots();
        let cands = self.storage.clear_file_candidates_range(cutoff_ms)?;
        let mut file_ids = Vec::new();
        let mut ai_att_files: Vec<(String, String)> = Vec::new();
        let mut deleted = 0u64;
        // 同一份文件可能被两条消息引用（直发 + 图片组各挂一次），
        // 候选里会出现两次——只删、只计一次，不虚增 toast 里的个数。
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (fid, path, mid) in &cands {
            if !self.path_is_managed(path, &roots) {
                continue;
            }
            if let Some(id) = fid {
                if !seen.insert(id.as_str()) {
                    continue;
                }
                if self.storage.file_referenced_outside_range(id, cutoff_ms)? {
                    continue;
                }
            }
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::log_line(
                        "storage",
                        &format!("清理相关文件失败（已跳过）：{path} {e}"),
                    );
                    continue;
                }
            }
            deleted += 1;
            if let Some(id) = fid {
                file_ids.push(id.clone());
            } else {
                // AI 附件按 (message_id, path) 回删（第三方评审）：只登记
                // 磁盘确已删除的那一份，同消息其他附件的登记行不受影响。
                ai_att_files.push((mid.clone(), path.clone()));
            }
        }
        self.storage
            .delete_cleared_files(&file_ids, &ai_att_files)?;
        Ok(deleted)
    }

    /// 删单会话内的关联文件（v1.0.2 ④）：口径与范围版一致，作用域改为
    /// 整个会话（不分时间）。返回实际删除的文件个数。
    fn delete_files_in_conversation(&self, conv_id: &str) -> Result<u64, ServiceError> {
        let roots = self.managed_roots();
        let cands = self.storage.clear_file_candidates_conversation(conv_id)?;
        let mut file_ids = Vec::new();
        let mut ai_att_files: Vec<(String, String)> = Vec::new();
        let mut deleted = 0u64;
        // 同上：同一 file_id 在候选里出现多次时只删、只计一次。
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (fid, path, mid) in &cands {
            if !self.path_is_managed(path, &roots) {
                continue;
            }
            if let Some(id) = fid {
                if !seen.insert(id.as_str()) {
                    continue;
                }
                if self
                    .storage
                    .file_referenced_outside_conversation(id, conv_id)?
                {
                    continue;
                }
            }
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    log::log_line(
                        "storage",
                        &format!("清理相关文件失败（已跳过）：{path} {e}"),
                    );
                    continue;
                }
            }
            deleted += 1;
            if let Some(id) = fid {
                file_ids.push(id.clone());
            } else {
                ai_att_files.push((mid.clone(), path.clone()));
            }
        }
        self.storage
            .delete_cleared_files(&file_ids, &ai_att_files)?;
        Ok(deleted)
    }

    /// #8（v2.6.6）：查看器内手动改名——用户给出的文件名+后缀即该
    /// 文件的最终身份：落库（消息正文与文件记录同步改），之后不再
    /// 被自动识别重猜。磁盘实体只对应用托管文件真改名（位于本机
    /// 数据目录内：接收文件在 downloads、暂存文档在 outbox；冲突
    /// 自动加 (n) 去重）；用户自有原文件（发送时引用、路径在数据
    /// 目录外）绝不碰磁盘、只改应用内身份。返回最终生效的文件名。
    pub fn rename_message_file(
        &self,
        message_id: &str,
        new_name: &str,
    ) -> Result<String, ServiceError> {
        let name = new_name.trim();
        if name.is_empty() {
            return Err(ServiceError::Other("文件名不能为空".into()));
        }
        // Windows 侧补强（评审）：`:` 可拼出 C:foo 驱动器相对路径
        // 跳出目标目录；尾随点/空格与保留名（CON/NUL/COM1…）会让
        // fs::rename 失败或行为异常，一并拒绝。
        let stem_up = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(
            stem_up.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
        if name.len() > 160
            || name.contains('/')
            || name.contains('\\')
            || name
                .chars()
                .any(|c| c.is_control() || ":*?\"<>|".contains(c))
            || name == "."
            || name == ".."
            || name.ends_with('.')
            || name.ends_with(' ')
            || reserved
        {
            return Err(ServiceError::Other("文件名不合法".into()));
        }
        let msg = self
            .storage
            .get_message(message_id)?
            .ok_or_else(|| ServiceError::Other("消息不存在".into()))?;
        let file_id = msg
            .file_id
            .clone()
            .ok_or_else(|| ServiceError::Other("该消息没有关联文件".into()))?;
        let mut meta = self
            .storage
            .get_file(&file_id)?
            .ok_or_else(|| ServiceError::Other("文件记录不存在".into()))?;
        let mut final_name = name.to_string();
        let mut renamed_from: Option<PathBuf> = None;
        let old_path = PathBuf::from(&meta.path);
        // 应用托管判定：路径在本机数据目录内（downloads/outbox 等）
        let data_root = self
            .default_download_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.download_dir());
        if !meta.path.is_empty() && old_path.starts_with(&data_root) && old_path.is_file() {
            if let Some(dir) = old_path.parent() {
                let mut target = dir.join(name);
                if target != old_path && target.exists() {
                    // 冲突去重：stem (n).ext
                    let (stem, ext) = match name.rfind('.') {
                        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
                        _ => (name, ""),
                    };
                    let mut n = 1u32;
                    loop {
                        let cand = if ext.is_empty() {
                            format!("{stem} ({n})")
                        } else {
                            format!("{stem} ({n}).{ext}")
                        };
                        target = dir.join(&cand);
                        if !target.exists() {
                            final_name = cand;
                            break;
                        }
                        n += 1;
                        if n > 999 {
                            return Err(ServiceError::Other("同名文件过多，改名失败".into()));
                        }
                    }
                }
                if target != old_path {
                    std::fs::rename(&old_path, &target)?;
                    renamed_from = Some(old_path.clone());
                    meta.path = target.to_string_lossy().to_string();
                }
            }
        }
        meta.name = final_name.clone();
        // 落库与磁盘改名尽量原子（评审补强）：任一落库失败就把
        // 磁盘名改回去，不许留下「磁盘新名、记录旧名」的孤儿。
        let db = (|| -> Result<(), ServiceError> {
            self.storage.save_file(&meta)?;
            self.storage.update_message_body(message_id, &final_name)?;
            Ok(())
        })();
        if let Err(e) = db {
            if let Some(from) = &renamed_from {
                let _ = std::fs::rename(&meta.path, from);
            }
            return Err(e);
        }
        log::log_line(
            "files",
            &format!(
                "文件已改名（查看器手动定名）：{} → {}",
                msg.body, final_name
            ),
        );
        Ok(final_name)
    }

    /// 在接收目录里定位一个已落地文件：根目录优先，其次按日分夹
    /// （#13 的 YYYY-MM-DD 子目录）。记录路径失效时的兜底定位用。
    fn find_received_file(&self, name: &str) -> Option<PathBuf> {
        let root = self.download_dir();
        let direct = root.join(name);
        if direct.exists() {
            return Some(direct);
        }
        if let Ok(rd) = std::fs::read_dir(&root) {
            // 日期目录按名倒序（最新日期优先）：同名文件多日各一份时
            // 取最近的那份（第三方评审采纳：read_dir 顺序不定）
            let mut dirs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .filter(|p| {
                    let dn = p.file_name().unwrap_or_default().to_string_lossy();
                    dn.len() == 10
                        && dn.as_bytes().get(4) == Some(&b'-')
                        && dn.as_bytes().get(7) == Some(&b'-')
                        && dn.chars().filter(|c| *c != '-').all(|c| c.is_ascii_digit())
                })
                .collect();
            dirs.sort();
            for p in dirs.iter().rev() {
                let cand = p.join(name);
                if cand.exists() {
                    return Some(cand);
                }
            }
        }
        None
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
        } else if let Some(found) = self.find_received_file(&meta.name) {
            Some(found)
        } else if !meta.path.is_empty() {
            Some(recorded)
        } else {
            None
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
            } else if let Some(found) = self.find_received_file(&meta.name) {
                Some(found)
            } else if !meta.path.is_empty() {
                Some(recorded)
            } else {
                None
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
    pub fn rename_ai_conversation(&self, conv_id: &str, title: &str) -> Result<(), ServiceError> {
        // v2.6.6 #10② 双保险：默认会话「AI 助手」不可改名（前端已禁用
        // 入口，这里再拦一道，报错文案与前端提示同口径）
        if conv_id == "ai-default" {
            return Err(ServiceError::Other("默认会话「AI 助手」不能改名".into()));
        }
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

    /// 删除一条 AI 会话（v2.6.2，#3）：连同全部消息一并清除、重启
    /// 不复活。默认会话「AI 助手」（ai-default）不可删——问AI 的固定
    /// 落点在它，删了会让落点悬空；设备会话不归这里管。
    pub fn delete_ai_conversation(&self, conv_id: &str) -> Result<(), ServiceError> {
        if conv_id == "ai-default" {
            return Err(ServiceError::Other("默认会话「AI 助手」不能删除".into()));
        }
        let conv = self
            .storage
            .list_conversations()?
            .into_iter()
            .find(|c| c.id == conv_id)
            .ok_or_else(|| ServiceError::Other("会话不存在".into()))?;
        if conv.kind != ConversationKind::Ai {
            return Err(ServiceError::Other("只能删除 AI 会话".into()));
        }
        self.storage.delete_conversation(conv_id)?;
        log::log_line(
            "ai",
            &format!("已删除 AI 会话「{}」（含全部消息）", conv.title),
        );
        Ok(())
    }

    /// 清空一条 AI 会话的全部聊天记录（v2.6.6，#10②；v1.0.2 ④ 加删
    /// 文件选项，与设置页范围清空同口径）：会话保留、只清消息。默认会
    /// 话「AI 助手」**放行**——这是它唯一允许的编辑操作（不可删/不可
    /// 改名/不可移动，记录可清）。delete_files=true 时连同本会话消息
    /// 关联的托管边界内实体文件（主要是 ai-att/ 下的附件副本）一并删除。
    /// 返回（删除消息条数，删除文件个数）。
    pub fn clear_ai_conversation_messages(
        &self,
        conv_id: &str,
        delete_files: bool,
    ) -> Result<(u64, u64), ServiceError> {
        let conv = self
            .storage
            .list_conversations()?
            .into_iter()
            .find(|c| c.id == conv_id)
            .ok_or_else(|| ServiceError::Other("会话不存在".into()))?;
        if conv.kind != ConversationKind::Ai {
            return Err(ServiceError::Other("只能清空 AI 会话的记录".into()));
        }
        let deleted_files = if delete_files {
            self.delete_files_in_conversation(conv_id)?
        } else {
            0
        };
        let n = self.storage.clear_conversation_messages(conv_id)?;
        log::log_line(
            "ai",
            &format!(
                "已清空 AI 会话「{}」的聊天记录 {n} 条，相关文件 {deleted_files} 个",
                conv.title
            ),
        );
        Ok((n, deleted_files))
    }

    /// ㉕ 解析 AI 页当前打开的会话：前端传了就校验必须是已存在的 Ai
    /// 会话，没传（或问AI 等固定落点）用 ai-default「AI 助手」。
    fn resolve_ai_conversation(&self, conv_id: Option<String>) -> Result<String, ServiceError> {
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
        attachments: Vec<AiAttachmentIn>,
    ) -> Result<Message, ServiceError> {
        let conv_id = self.resolve_ai_conversation(conversation_id)?;
        self.ai_push_and_answer(&conv_id, text, thinking, &attachments)
            .await
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
        self.ai_push_and_answer(&ai_conv, &body, thinking, &[])
            .await
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
    /// AI 文本附件展开（v1.0.1）：把一条消息变成送模型的完整内容——
    /// 正文之后逐个拼「附件：文件名 + 代码块包裹的内容」。本机调用与
    /// 对方转发共用 assemble_ai_context 的组装结果，附件内容随问题走
    /// 既有通道、不另传文件实体。附件文件缺失时给占位说明，不静默丢。
    fn ai_content_of(m: &Message, att_map: &HashMap<String, Vec<AiAttachment>>) -> String {
        let Some(list) = att_map.get(&m.id) else {
            return m.body.clone();
        };
        let mut s = m.body.clone();
        for a in list {
            let content = std::fs::read_to_string(&a.path)
                .unwrap_or_else(|_| "（附件内容文件缺失）".to_string());
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(&format!("附件：{}\n```\n{}\n```", a.name, content));
        }
        s
    }

    async fn assemble_ai_context(
        self: &Arc<Self>,
        conv_id: &str,
    ) -> Result<Vec<ChatMessage>, ServiceError> {
        let all = self.storage.list_messages(conv_id, None, 100_000)?;
        // 附件按消息分组（没有附件的会话这里是空表，行为与旧版一致）
        let mut att_map: HashMap<String, Vec<AiAttachment>> = HashMap::new();
        for r in self.storage.ai_attachments_for_conversation(conv_id)? {
            att_map.entry(r.message_id.clone()).or_default().push(r);
        }
        let expanded: Vec<String> = all
            .iter()
            .map(|m| Self::ai_content_of(m, &att_map))
            .collect();
        let bodies: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();
        let kept_from = ai_kept_from(&bodies);
        let dropped = &all[..kept_from];
        let kept = &all[kept_from..];
        let kept_tokens: usize = expanded[kept_from..]
            .iter()
            .map(|b| estimate_tokens(b) + 4)
            .sum();
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
            if let Some(summary) = self.ai_summary_for(conv_id, dropped, &att_map).await {
                out.push(ChatMessage {
                    role: "system".into(),
                    content: format!("此前对话摘要：{summary}"),
                });
            }
        }
        for (i, m) in kept.iter().enumerate() {
            out.push(ChatMessage {
                role: if m.sender == Sender::Ai {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: expanded[kept_from + i].clone(),
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
        att_map: &HashMap<String, Vec<AiAttachment>>,
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
            let who = if m.sender == Sender::Ai {
                "AI"
            } else {
                "用户"
            };
            src.push_str(&format!("{who}：{}\n", Self::ai_content_of(m, att_map)));
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
        attachments: &[AiAttachmentIn],
    ) -> Result<Message, ServiceError> {
        // AI 文本附件第二道闸（v1.0.1）：前端已验，这里防绕过——
        // 单件/总量/件数任一超限整条拒绝，绝不静默截断。
        if !attachments.is_empty() {
            if attachments.len() > AI_ATT_MAX_COUNT {
                return Err(ServiceError::Other(format!(
                    "附件最多 {AI_ATT_MAX_COUNT} 个"
                )));
            }
            let total: usize = attachments.iter().map(|a| a.content.len()).sum();
            if total > AI_ATT_MAX_TOTAL {
                return Err(ServiceError::Other("附件总大小超过 512 KB 上限".into()));
            }
            if let Some(a) = attachments
                .iter()
                .find(|a| a.content.len() > AI_ATT_MAX_SINGLE)
            {
                return Err(ServiceError::Other(format!(
                    "附件「{}」超过 512 KB 上限",
                    a.name
                )));
            }
        }
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
        // AI 文本附件落盘 + 登记（v1.0.1）：内容副本存数据目录
        // ai-att/<消息 id>/ 下，重启后文件片与「点开看内容」靠它；
        // 消息正文只留用户输入的文字，内容在组装上下文时才拼入。
        if !attachments.is_empty() {
            let dir = self
                .default_download_dir
                .parent()
                .map(|p| p.join("ai-att").join(&user_msg.id))
                .unwrap_or_else(|| PathBuf::from("ai-att").join(&user_msg.id));
            std::fs::create_dir_all(&dir)?;
            for (i, a) in attachments.iter().enumerate() {
                let base = a
                    .name
                    .replace(['/', '\\'], "_")
                    .trim()
                    .trim_start_matches('.')
                    .to_string();
                let base = if base.is_empty() {
                    format!("附件{}", i + 1)
                } else {
                    base
                };
                let path = dir.join(format!("{}-{}", i + 1, base));
                std::fs::write(&path, &a.content)?;
                self.storage.insert_ai_attachment(
                    &user_msg.id,
                    &a.name,
                    a.content.len() as i64,
                    &path.to_string_lossy(),
                )?;
            }
            log_line(
                "ai",
                &format!(
                    "essential: AI 文本附件已登记（消息 {}，{} 个）",
                    user_msg.id,
                    attachments.len()
                ),
            );
        }
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
                let applied_of = |applied: bool| opts.thinking.map(|_| applied);
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
        let outcome = self
            .chat_core_src(msgs, Some(&mut cb), source, opts)
            .await?;
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
        msgs: &[RichMessage],
        opts: &AiCallOptions,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<String, ServiceError> {
        log::log_line("api", &format!("本地 API 网关请求：{} 条消息", msgs.len()));
        let outcome = self.gateway_core(msgs, on_delta, opts).await.map_err(|e| {
            log::log_line("api", &format!("本地 API 网关请求失败：{e}"));
            e
        })?;
        Ok(outcome.text)
    }

    /// 网关富消息路由（v2.6.2，#13）：与 chat_core_src 的 Auto 路由同构
    /// （本机配置优先、否则经对端转发），但全程保留消息完整形态
    /// （tool_calls / tool_call_id / content 原样），并在真正调用上游
    /// 的一侧经 normalize_messages 规范化——压平链路曾把 TRAE 的
    /// 工具调用历史变成上游硬拒的形态，长历史必 422 即源于此。
    async fn gateway_core(
        &self,
        msgs: &[RichMessage],
        on_delta: &mut (dyn FnMut(&str) + Send),
        opts: &AiCallOptions,
    ) -> Result<AiOutcome, ServiceError> {
        let cfg = self.ai_cfg.lock().await.clone();
        if let Some(cfg) = cfg.filter(|c| c.is_ready()) {
            log::log_line(
                "ai",
                &format!(
                    "AI 网关请求路由：本机接口（模型 {}，{} 条消息）",
                    cfg.model,
                    msgs.len()
                ),
            );
            let client = AiClient::new(cfg.clone());
            let applied_of = |applied: bool| opts.thinking.map(|_| applied);
            if cfg.stream {
                let (full, applied) = client
                    .chat_rich_stream_opts(msgs, opts, |d| on_delta(d))
                    .await
                    .map_err(|e| {
                        log::log_line("ai", &format!("AI 网关请求失败（本机接口）：{e}"));
                        e
                    })?;
                return Ok(AiOutcome {
                    text: full,
                    thinking_applied: applied_of(applied),
                });
            }
            let (full, applied) = client.chat_rich_once_opts(msgs, opts).await.map_err(|e| {
                log::log_line("ai", &format!("AI 网关请求失败（本机接口）：{e}"));
                e
            })?;
            on_delta(&full);
            return Ok(AiOutcome {
                text: full,
                thinking_applied: applied_of(applied),
            });
        }
        if self.is_connected().await {
            log::log_line(
                "ai",
                &format!("AI 网关请求路由：经对方设备转发（{} 条消息）", msgs.len()),
            );
            return self
                .proxy_to_peer_rich(msgs, Some(on_delta), opts)
                .await
                .map_err(|e| {
                    log::log_line("ai", &format!("AI 网关请求失败（对端转发）：{e}"));
                    e
                });
        }
        Err(bluelink_ai::AiError::NotConfigured.into())
    }

    /// 富消息版对端转发：消息经 serde 原样过线（字段一个不少），
    /// 接收与等待逻辑与 proxy_to_peer 相同。
    async fn proxy_to_peer_rich(
        &self,
        msgs: &[RichMessage],
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
            .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
            .collect();
        // 发送失败必须先摘掉 waiter 再返回（评审修正）：旧版 `?` 直接
        // 冒泡，proxy_waiters 里永久残留一条死等者，失败一次漏一条。
        if let Err(e) = session
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
            .await
        {
            self.proxy_waiters.lock().await.remove(&request_id);
            return Err(e.into());
        }
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
        // 发送失败必须先摘掉 waiter 再返回（评审修正）：旧版 `?` 直接
        // 冒泡，proxy_waiters 里永久残留一条死等者，失败一次漏一条。
        if let Err(e) = session
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
            .await
        {
            self.proxy_waiters.lock().await.remove(&request_id);
            return Err(e.into());
        }
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
        // 富消息解析（v2.6.2，#13）：tool_calls/tool_call_id/content
        // 原样保留，规范化在 AiClient 组装请求体时统一做；解析不出的
        // 退回 role+文本的宽容形态，绝不因一条坏消息拒整段请求。
        let msgs: Vec<RichMessage> = messages
            .iter()
            .map(|v| {
                serde_json::from_value::<RichMessage>(v.clone()).unwrap_or_else(|_| {
                    RichMessage::text(
                        v.get("role").and_then(|r| r.as_str()).unwrap_or("user"),
                        &bluelink_ai::rich_content_text(v.get("content")),
                    )
                })
            })
            .collect();
        let prompt_chars: u64 = msgs
            .iter()
            .map(|m| {
                bluelink_ai::rich_content_text(m.content.as_ref())
                    .chars()
                    .count() as u64
            })
            .sum();
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
            .chat_rich_stream_opts(&msgs, opts, |delta| {
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

#[cfg(test)]
mod clear_files_tests {
    use super::*;

    fn temp_node(name: &str) -> (Arc<Node>, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("bl-clear-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = Node::new(&base, name).expect("建节点");
        (node, base)
    }

    fn add_file_msg(node: &Node, conv: &str, mid: &str, fid: &str, path: &Path, created_at: i64) {
        node.storage
            .save_file(&FileMeta {
                id: fid.to_string(),
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                size: 4,
                sha256: String::new(),
                path: path.to_string_lossy().to_string(),
                mime: None,
            })
            .unwrap();
        node.storage
            .insert_message(&Message {
                id: mid.to_string(),
                conversation_id: conv.to_string(),
                sender: Sender::Peer,
                kind: MessageKind::File,
                body: "文件.txt".to_string(),
                file_id: Some(fid.to_string()),
                reply_to: None,
                status: MessageStatus::Received,
                created_at,
                seq: 0,
            })
            .unwrap();
    }

    fn add_conv(node: &Node, id: &str, kind: ConversationKind) {
        node.storage
            .create_conversation(&Conversation {
                id: id.to_string(),
                kind,
                peer_device_id: None,
                title: "会话".to_string(),
                draft: String::new(),
                last_read_id: None,
                created_at: 0,
            })
            .unwrap();
    }

    /// ④ 范围清空 + 删文件：下载目录内的删磁盘+删记录，数据目录外的
    /// 用户文件不动，范围外的消息及其文件不动；不勾删文件时磁盘全不动。
    #[test]
    fn 范围清空删文件只动托管内且只动范围内() {
        let (node, base) = temp_node("范围");
        let dl = node.download_dir();
        std::fs::create_dir_all(&dl).unwrap();
        // 数据目录外的用户文件（与 base 同级、另一棵树）
        let outside = std::env::temp_dir().join(format!("bl-clear-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("用户自有.txt");
        std::fs::write(&outside_file, "别碰我").unwrap();
        add_conv(&node, "c1", ConversationKind::Peer);
        let now = now_ms();
        // 范围内（近 24h）：托管内文件 + 目录外文件各一
        let in_file = dl.join("在内.txt");
        std::fs::write(&in_file, "删我").unwrap();
        add_file_msg(&node, "c1", "m-in", "f-in", &in_file, now);
        add_file_msg(&node, "c1", "m-out", "f-out", &outside_file, now);
        // 范围外（30 天前）：托管内文件，应保留
        let old_file = dl.join("旧文件.txt");
        std::fs::write(&old_file, "留我").unwrap();
        add_file_msg(
            &node,
            "c1",
            "m-old",
            "f-old",
            &old_file,
            now - 30 * 24 * 3600 * 1000,
        );
        // 不勾：只清记录、不动磁盘
        let (n, f) = node.clear_chat_history_range("24h", false).unwrap();
        assert_eq!((n, f), (2, 0));
        assert!(in_file.exists(), "不勾删文件时磁盘文件必须保留");
        // 勾上：范围内的托管内文件删、目录外文件留、范围外文件留
        // 注意：上一轮已把 m-in/m-out 的消息删掉，需重建一轮再测勾选
        add_file_msg(&node, "c1", "m-in2", "f-in", &in_file, now);
        add_file_msg(&node, "c1", "m-out2", "f-out", &outside_file, now);
        let (n, f) = node.clear_chat_history_range("24h", true).unwrap();
        assert_eq!((n, f), (2, 1), "只删 1 个托管内文件");
        assert!(!in_file.exists(), "托管内文件应已删除");
        assert!(outside_file.exists(), "数据目录外的用户文件绝不碰");
        assert!(old_file.exists(), "范围外的文件不动");
        assert!(
            node.storage.get_file("f-in").unwrap().is_none(),
            "已删文件的记录应回删"
        );
        assert!(
            node.storage.get_file("f-out").unwrap().is_some(),
            "未删文件的记录保留"
        );
        assert!(
            node.storage.get_file("f-old").unwrap().is_some(),
            "范围外文件的记录保留"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    fn add_group_msg(
        node: &Node,
        conv: &str,
        mid: &str,
        items: &[(String, String)],
        shared_fid: &str,
        created_at: i64,
    ) {
        // items: (file_id, msg_id)；shared_fid 非空时追加为最后一项。
        let mut arr: Vec<serde_json::Value> = items
            .iter()
            .enumerate()
            .map(|(idx, (fid, mid2))| {
                serde_json::json!({"msg_id": mid2, "file_id": fid, "name": "图.png", "size": 2, "sha256": "", "idx": idx})
            })
            .collect();
        if !shared_fid.is_empty() {
            let idx = arr.len();
            arr.push(serde_json::json!({"msg_id": format!("{mid}-shared"), "file_id": shared_fid, "name": "共享.png", "size": 2, "sha256": "", "idx": idx}));
        }
        let body = serde_json::json!({"v": 1, "items": arr}).to_string();
        node.storage
            .insert_message(&Message {
                id: mid.to_string(),
                conversation_id: conv.to_string(),
                sender: Sender::Peer,
                kind: MessageKind::ImageGroup,
                body,
                file_id: None,
                reply_to: None,
                status: MessageStatus::Received,
                created_at,
                seq: 0,
            })
            .unwrap();
    }

    /// ④ 图片组补漏：组消息 file_id 为 NULL，图在 body JSON 的 items
    /// 里——范围清空勾选删文件时整组图都要删；范围外组里的图、
    /// 被范围外组共享的图不动（守卫同样要认组 JSON 里的引用）。
    #[test]
    fn 范围清空删文件含图片组() {
        let (node, base) = temp_node("图片组");
        let dl = node.download_dir();
        std::fs::create_dir_all(&dl).unwrap();
        add_conv(&node, "c1", ConversationKind::Peer);
        let now = now_ms();
        let mut items: Vec<(String, String)> = Vec::new();
        for i in 0..4 {
            let p = dl.join(format!("g{i}.png"));
            std::fs::write(&p, "图").unwrap();
            let fid = format!("gf-{i}");
            node.storage
                .save_file(&FileMeta {
                    id: fid.clone(),
                    name: format!("g{i}.png"),
                    size: 2,
                    sha256: String::new(),
                    path: p.to_string_lossy().to_string(),
                    mime: None,
                })
                .unwrap();
            items.push((fid, format!("gm-{i}")));
        }
        let shared = dl.join("共享.png");
        std::fs::write(&shared, "共享").unwrap();
        node.storage
            .save_file(&FileMeta {
                id: "gf-shared".to_string(),
                name: "共享.png".to_string(),
                size: 2,
                sha256: String::new(),
                path: shared.to_string_lossy().to_string(),
                mime: None,
            })
            .unwrap();
        // 范围内的组（含 4 张图 + 共享图）；范围外的组（30 天前）同样挂着共享图
        add_group_msg(&node, "c1", "g-now", &items, "gf-shared", now);
        add_group_msg(
            &node,
            "c1",
            "g-old",
            &[],
            "gf-shared",
            now - 30 * 24 * 3600 * 1000,
        );
        let (n, f) = node.clear_chat_history_range("24h", true).unwrap();
        assert_eq!(
            (n, f),
            (1, 4),
            "只删范围内组的 4 张图，共享图因范围外引用保留"
        );
        for i in 0..4 {
            assert!(!dl.join(format!("g{i}.png")).exists(), "组内图应已删除");
            assert!(
                node.storage.get_file(&format!("gf-{i}")).unwrap().is_none(),
                "组内图记录应回删"
            );
        }
        assert!(shared.exists(), "被范围外组共享的图不动");
        assert!(
            node.storage.get_file("gf-shared").unwrap().is_some(),
            "共享图记录保留"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ④ 单会话清空 + 删文件：AI 会话的 ai-att 副本（数据目录内）
    /// 随清空删除；另一会话的附件不动。
    #[test]
    fn 单会话清空删文件只动本会话附件() {
        let (node, base) = temp_node("单会话");
        add_conv(&node, "ai-1", ConversationKind::Ai);
        add_conv(&node, "ai-2", ConversationKind::Ai);
        for (conv, mid) in [("ai-1", "am-1"), ("ai-2", "am-2")] {
            node.storage
                .insert_message(&Message {
                    id: mid.to_string(),
                    conversation_id: conv.to_string(),
                    sender: Sender::Me,
                    kind: MessageKind::Text,
                    body: "问题".to_string(),
                    file_id: None,
                    reply_to: None,
                    status: MessageStatus::Sent,
                    created_at: now_ms(),
                    seq: 0,
                })
                .unwrap();
            let dir = base.join("ai-att").join(mid);
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("附件.txt");
            std::fs::write(&p, "附件内容").unwrap();
            node.storage
                .insert_ai_attachment(mid, "附件.txt", 12, &p.to_string_lossy())
                .unwrap();
        }
        let (n, f) = node.clear_ai_conversation_messages("ai-1", true).unwrap();
        assert_eq!((n, f), (1, 1));
        assert!(
            !base.join("ai-att").join("am-1").join("附件.txt").exists(),
            "本会话附件副本应删除"
        );
        assert!(
            base.join("ai-att").join("am-2").join("附件.txt").exists(),
            "另一会话附件不动"
        );
        assert_eq!(
            node.storage.list_ai_attachments("am-1").unwrap().len(),
            0,
            "已删附件的登记行应回删"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ④ 回归（第三方评审，严重）：「全部」清空 + 删文件曾几乎一个文件
    /// 都删不掉——范围外引用守卫在 cutoff=None 时恒真。修后守卫直接
    /// 返回 false（全部消息都会被删，不存在范围外引用）。
    #[test]
    fn 全部清空删文件不再被守卫挡住() {
        let (node, base) = temp_node("全部");
        let dl = node.download_dir();
        std::fs::create_dir_all(&dl).unwrap();
        add_conv(&node, "c1", ConversationKind::Peer);
        let now = now_ms();
        let f1 = dl.join("全1.txt");
        let f2 = dl.join("全2.txt");
        std::fs::write(&f1, "一").unwrap();
        std::fs::write(&f2, "二").unwrap();
        add_file_msg(&node, "c1", "m-1", "f-1", &f1, now);
        add_file_msg(&node, "c1", "m-2", "f-2", &f2, now);
        let (n, f) = node.clear_chat_history_range("all", true).unwrap();
        assert_eq!((n, f), (2, 2), "全部清空应删掉两个托管内文件");
        assert!(!f1.exists() && !f2.exists(), "磁盘文件应已删除");
        assert!(node.storage.get_file("f-1").unwrap().is_none());
        assert!(node.storage.get_file("f-2").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ④ 回归（第三方评审，严重）：一条消息挂两个 AI 附件，其中一个
    /// 磁盘删除失败时，登记行须按 (message_id, path) 精确回删——旧版
    /// 按 message_id 整行删会把失败那个附件的登记行也删掉，留下孤儿文件。
    /// 用「附件路径是个目录」模拟删除失败（remove_file 删不动目录）。
    #[test]
    fn 同消息多附件其一删失败时登记行按路径回删() {
        let (node, base) = temp_node("多附件");
        add_conv(&node, "ai-1", ConversationKind::Ai);
        node.storage
            .insert_message(&Message {
                id: "am-1".to_string(),
                conversation_id: "ai-1".to_string(),
                sender: Sender::Me,
                kind: MessageKind::Text,
                body: "问题".to_string(),
                file_id: None,
                reply_to: None,
                status: MessageStatus::Sent,
                created_at: now_ms(),
                seq: 0,
            })
            .unwrap();
        let dir = base.join("ai-att").join("am-1");
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("好附件.txt");
        std::fs::write(&good, "附件内容").unwrap();
        let bad = dir.join("坏目录");
        std::fs::create_dir_all(&bad).unwrap();
        node.storage
            .insert_ai_attachment("am-1", "好附件.txt", 12, &good.to_string_lossy())
            .unwrap();
        node.storage
            .insert_ai_attachment("am-1", "坏目录", 0, &bad.to_string_lossy())
            .unwrap();
        let (n, f) = node.clear_ai_conversation_messages("ai-1", true).unwrap();
        assert_eq!((n, f), (1, 1), "只计一次成功删除");
        assert!(!good.exists(), "能删的附件应删除");
        assert!(bad.exists(), "删不动的目录原样保留");
        let left = node.storage.list_ai_attachments("am-1").unwrap();
        assert_eq!(left.len(), 1, "只回删成功那份的登记行");
        assert_eq!(left[0].path, bad.to_string_lossy(), "失败附件的登记行保留");
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn temp_node(name: &str) -> Arc<Node> {
        let dir =
            std::env::temp_dir().join(format!("bl-reconnect-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Node::new(&dir, name).expect("建节点")
    }

    /// 自动重连 rig 证据（v2.6.2）：A 经 TCP 连 B 并登记路线与拨号器；
    /// B 侧会话被杀（A 视角为意外中断）后，A 应在退避节奏内自动重连
    /// 成功并在同一会话继续发消息；A 主动断开后则绝不重连。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 意外断线自动重连且主动断开不重连() {
        let a = temp_node("重连甲");
        let b = temp_node("重连乙");
        // B 持续监听并接管每条入站连接（模拟对端应用一直在运行）
        let listener = bluelink_transport::listen_tcp("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let b_host = b.clone();
        let accept_alive = Arc::new(AtomicBool::new(true));
        let alive2 = accept_alive.clone();
        tokio::spawn(async move {
            while alive2.load(Ordering::Relaxed) {
                match bluelink_transport::accept_tcp(&listener).await {
                    Ok(conn) => {
                        let _ = b_host.attach(conn, true, "").await;
                    }
                    Err(_) => break,
                }
            }
        });
        // A 的拨号器：按 tcp 路线回拨（与应用层拨号器同构的最小版）
        let dialer: ReconnectDialer = Arc::new(|node, target, gen| {
            Box::pin(async move {
                let addr = target
                    .route
                    .strip_prefix("tcp:")
                    .ok_or("路线错误")?
                    .to_string();
                let conn = bluelink_transport::connect_tcp(&addr)
                    .await
                    .map_err(|e| e.to_string())?;
                if !node.reconnect_should_continue(gen) {
                    drop(conn);
                    return Err("已取消".to_string());
                }
                node.attach(conn, false, &target.code)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
        });
        a.set_reconnect_dialer(dialer).await;
        // A 首次连接（登记路线 → attach 消费为重连目标）
        a.set_pending_route(format!("tcp:{addr}")).await;
        let conn = bluelink_transport::connect_tcp(&addr).await.unwrap();
        let code = bluelink_domain::connection_code(&b.device_id);
        a.attach(conn, false, &code).await.unwrap();
        assert!(a.is_connected().await);
        a.send_text("断前消息", None).await.unwrap();
        // B 侧杀掉会话（走 Fault 收尾、不发 Goodbye，模拟进程被杀/
        // 链路被拔）：A 收到 EOF = 意外中断，应触发自动重连。
        // 注：v2.6.6 起不能再用 b.disconnect() 模拟意外——那是用户
        // 主动断开、会先发 Goodbye，A 按 #17 本就不许重连（见下方
        // 双向验证段）。
        let b_session = b.peer.lock().await.clone().expect("B 应有会话");
        b.end_session(&b_session, "测试模拟意外中断", SessionEnd::Fault)
            .await;
        // 退避首拍 2 秒 + 拨号，给足 20 秒观察窗
        let mut reconnected = false;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if a.is_connected().await && b.is_connected().await {
                reconnected = true;
                break;
            }
        }
        assert!(reconnected, "意外断线后 20 秒内应自动重连成功");
        // 同一会话继续发消息、B 应收到
        a.send_text("重连后消息", None).await.unwrap();
        let mut got = false;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let msgs = b
                .storage
                .list_messages(&format!("peer-{}", a.device_id), None, 50)
                .unwrap_or_default();
            if msgs.iter().any(|m| m.body == "重连后消息") {
                got = true;
                break;
            }
        }
        assert!(got, "重连后消息应送达对端");
        // A 主动断开：绝不触发重连（等过首拍退避 2 秒再断言）
        a.disconnect().await;
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        assert!(!a.is_connected().await, "用户主动断开后不许自动重连");
        // #17 双向验证（v2.6.6，上轮盲区补验）：重新连上后改由 B
        // 用户主动断开——B 的 disconnect 会先发 Goodbye，A 收到后
        // 本次断开必须按「对方主动断开」分类、绝不自动连回。
        a.set_pending_route(format!("tcp:{addr}")).await;
        let conn = bluelink_transport::connect_tcp(&addr).await.unwrap();
        a.attach(conn, false, &code).await.unwrap();
        assert!(a.is_connected().await && b.is_connected().await);
        b.disconnect().await;
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        assert!(
            !a.is_connected().await,
            "对方主动断开（Goodbye）后不许自动重连（#17）"
        );
        assert!(!b.is_connected().await);
        accept_alive.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod clip_state_tests {
    use super::*;

    fn img(w: u32, h: u32, fill: u8) -> ClipContent {
        ClipContent::Image {
            width: w,
            height: h,
            rgba: vec![fill; (w * h * 4) as usize],
        }
    }

    /// #16 核心回归（2026-11-22 双机日志复刻）：A 复制 → 发送；
    /// B 应用后，本机读回的内容经系统格式转换（文字 \n→\r\n、
    /// 图片 alpha 通道翻转）字节已变——B 的监视必须认出这是同步
    /// 来的（SyncEcho、绝不回发）；万一回声仍漏到 A，A 的 apply
    /// 必须按发送登记丢弃（EchoDrop、绝不写入）。
    #[test]
    fn 格式转换回声被状态机拦截() {
        // A：用户复制文字 → 裁决发送 → 登记
        let mut a = ClipGuard::default();
        let text = ClipContent::Text("第一行\n第二行".into());
        assert!(matches!(a.poll_decide(&text, None), ClipPollDecision::Send));
        a.note_sent(text.hash());
        // A 再读到同一内容：无变化
        assert!(matches!(
            a.poll_decide(&text, None),
            ClipPollDecision::Unchanged
        ));

        // B：收到 → 裁决写入 → 登记（回读哈希 = 转换形态的哈希）
        let mut b = ClipGuard::default();
        assert!(matches!(b.apply_decide(&text), ClipApplyDecision::Proceed));
        let readback = ClipContent::Text("第一行\r\n第二行".into());
        b.note_applied(text.clone(), Some(readback.hash()));
        // B 监视读到转换形态：同步回声，不发送
        assert!(matches!(
            b.poll_decide(&readback, None),
            ClipPollDecision::SyncEcho
        ));
        // B 监视读到原形态：同样认得出
        assert!(matches!(
            b.poll_decide(&text, None),
            ClipPollDecision::SyncEcho
        ));

        // 假设回声仍漏回 A：A 按发送登记丢弃，绝不写入（历史不重复）
        assert!(matches!(a.apply_decide(&text), ClipApplyDecision::EchoDrop));
        // A 收到 B 的等价转换形态（线缆哈希不同）：靠基线/应用账判本机已有
        let mut a2 = ClipGuard::default();
        a2.note_applied(text.clone(), Some(text.hash()));
        assert!(matches!(
            a2.apply_decide(&readback),
            ClipApplyDecision::AlreadyLocal
        ));
    }

    /// #16 图片路径：alpha 通道被系统翻转（0↔255）+ 个别字节微差，
    /// 仍判等价；整图行翻转（真差异）不许误认。
    #[test]
    fn 图片格式转换等价判定() {
        let orig = img(4, 3, 7);
        let mut converted = img(4, 3, 7);
        if let ClipContent::Image { rgba, .. } = &mut converted {
            for i in (3..rgba.len()).step_by(4) {
                rgba[i] = 255; // alpha 全翻转
            }
            rgba[0] = 9; // 个别字节微差（预算内）
        }
        assert!(clip_equiv(&orig, &converted));
        let mut b = ClipGuard::default();
        b.note_applied(orig.clone(), Some(converted.hash()));
        assert!(matches!(
            b.poll_decide(&converted, None),
            ClipPollDecision::SyncEcho
        ));

        // 行翻转 = 内容真变了：不许认成同步回声
        let mut flipped = img(4, 3, 7);
        if let ClipContent::Image { rgba, .. } = &mut flipped {
            let row = 4 * 4usize;
            let mut rows: Vec<Vec<u8>> = rgba.chunks(row).map(|c| c.to_vec()).collect();
            rows.reverse();
            *rgba = rows.concat();
            // 填充值全同翻转看不出，改成行间不同填充再翻
            for (ri, r) in rgba.chunks_mut(row).enumerate() {
                for byte in r.iter_mut() {
                    *byte = (ri * 40) as u8;
                }
            }
        }
        let mut orig2 = img(4, 3, 0);
        if let ClipContent::Image { rgba, .. } = &mut orig2 {
            let row = 4 * 4usize;
            for (ri, r) in rgba.chunks_mut(row).enumerate() {
                for byte in r.iter_mut() {
                    *byte = ((2 - ri) * 40) as u8;
                }
            }
        }
        assert!(!clip_equiv(&orig2, &flipped), "行序不同的图片不许判等价");
    }

    /// #16 发送登记有界：挤掉最旧的后，老回声不再拦截（可接受的
    /// 边界），新登记始终有效；用户真新复制永远判 Send。
    #[test]
    fn 发送登记有界且新内容放行() {
        let mut g = ClipGuard::default();
        let mut hashes = Vec::new();
        for i in 0..(CLIP_SENT_CAP + 4) {
            let c = ClipContent::Text(format!("内容{i}"));
            hashes.push(c.hash());
            g.note_sent(c.hash());
        }
        assert_eq!(g.sent.len(), CLIP_SENT_CAP);
        let newest = ClipContent::Text(format!("内容{}", CLIP_SENT_CAP + 3));
        assert!(matches!(
            g.apply_decide(&newest),
            ClipApplyDecision::EchoDrop
        ));
        let fresh = ClipContent::Text("全新内容".into());
        assert!(matches!(
            g.poll_decide(&fresh, None),
            ClipPollDecision::Send
        ));
        assert!(matches!(g.apply_decide(&fresh), ClipApplyDecision::Proceed));
    }
}

#[cfg(test)]
mod clip_bounded_and_batch_tests {
    use super::*;

    /// 任务一核心机制：带截止的调用——快闭包拿到结果，慢闭包在
    /// 预算内返回 None（降级），绝不陪它等完。
    #[test]
    fn 截止包装快慢两路() {
        let fast = call_with_deadline(std::time::Duration::from_millis(500), || 42);
        assert_eq!(fast, Some(42));
        let start = std::time::Instant::now();
        let slow = call_with_deadline(std::time::Duration::from_millis(100), || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            7
        });
        assert_eq!(slow, None, "超时必须降级为 None");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "降级必须在预算量级内返回，不能陪慢闭包等完"
        );
    }

    /// 任务一锁审计：锁被长时间持有时，带截止的获取等满即放弃；
    /// 锁空闲时立即拿到。
    #[test]
    fn 带截止锁不无限排队() {
        let m = std::sync::Mutex::new(1);
        let free = lock_with_deadline(&m, std::time::Duration::from_millis(500));
        assert!(free.is_some());
        drop(free);
        let _held = m.lock().unwrap();
        let start = std::time::Instant::now();
        let blocked = lock_with_deadline(&m, std::time::Duration::from_millis(100));
        assert!(blocked.is_none(), "锁被占时等满截止必须放弃");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    /// 历史缓冲：相邻同内容不重复记、超容挤最旧、取最新 N 条为
    /// 时间正序（旧→新）。
    #[test]
    fn 历史缓冲去重限容与正序() {
        let mut g = ClipGuard::default();
        let a = ClipContent::Text("甲".into());
        g.note_history(a.clone());
        g.note_history(a.clone()); // 相邻重复：不记第二遍
        assert_eq!(g.history.len(), 1);
        for i in 0..12 {
            g.note_history(ClipContent::Text(format!("条目{i}")));
        }
        assert_eq!(g.history.len(), CLIP_HISTORY_CAP, "超容必须挤最旧");
        let latest3 = g.history_latest(3);
        assert_eq!(latest3.len(), 3);
        assert_eq!(latest3[0], ClipContent::Text("条目9".into()));
        assert_eq!(latest3[2], ClipContent::Text("条目11".into()));
        assert_eq!(g.history_latest(99).len(), CLIP_HISTORY_CAP);
    }

    /// 多条整批：对方按序应用 3 条后，单槽 applied 只剩最后一条。
    /// poll 侧整批账覆盖全部——读到整批中任何一条都是同步回声，
    /// 绝不回发（方向规矩）。apply 侧自复测 #6 起改口径：只有剪
    /// 贴板正装着的那一条（最后一条）判已有，前两条重发到达判
    /// 真更新照常写入——否则原机 A→B→A 时接收机变不回 A。
    #[test]
    fn 整批应用后每条都被回声抑制覆盖() {
        let mut b = ClipGuard::default();
        let items: Vec<ClipContent> = (0..3)
            .map(|i| ClipContent::Text(format!("第{i}条")))
            .collect();
        for it in &items {
            assert!(matches!(b.apply_decide(it), ClipApplyDecision::Proceed));
            b.note_applied(it.clone(), Some(it.hash()));
        }
        // apply 侧（先查，apply_decide 不改状态）：剪贴板停在最后
        // 一条上，它判已有；前两条判真更新
        assert!(
            matches!(b.apply_decide(&items[2]), ClipApplyDecision::AlreadyLocal),
            "剪贴板正装着的整批最后一条应判本机已有"
        );
        for it in &items[..2] {
            assert!(
                matches!(b.apply_decide(it), ClipApplyDecision::Proceed),
                "剪贴板已换内容时整批旧条目 {} 重发到达应照常写入（#6）",
                it.hash()
            );
        }
        // poll 侧：整批中任何一条被读到都是同步回声，绝不回发
        for it in &items {
            assert!(
                matches!(b.poll_decide(it, None), ClipPollDecision::SyncEcho),
                "整批中的 {} 未被 poll 侧回声抑制覆盖",
                it.hash()
            );
        }
    }

    /// 复测 #6+#8 回归（用户定案的方向规矩）：机器1 复制 A、B、A，
    /// 机器2 依次接收——第三份 A 到达时机器2 剪贴板停在 B，必须
    /// 照常写入变回 A；而机器2 上的 A、B 永远不许回发到机器1。
    #[test]
    fn 接收机不回发且原机重发旧内容照常写入() {
        let a = ClipContent::Text("内容A".into());
        let b_content = ClipContent::Text("内容B".into());
        let mut rx = ClipGuard::default();
        // A、B 先后同步到达并写入
        assert!(matches!(rx.apply_decide(&a), ClipApplyDecision::Proceed));
        rx.note_applied(a.clone(), Some(a.hash()));
        assert!(matches!(
            rx.apply_decide(&b_content),
            ClipApplyDecision::Proceed
        ));
        rx.note_applied(b_content.clone(), Some(b_content.hash()));
        // 第三份 A 到达：剪贴板停在 B，判真更新（旧口径凭应用账
        // 里有 A 判已有丢弃——这就是 A→B→A 第三份同步不了的真因）
        assert!(
            matches!(rx.apply_decide(&a), ClipApplyDecision::Proceed),
            "原机重发的 A 被接收侧应用账误吞（#6）"
        );
        rx.note_applied(a.clone(), Some(a.hash()));
        // 写入后剪贴板停在 A：再来一份 A 判已有，不重写
        assert!(matches!(
            rx.apply_decide(&a),
            ClipApplyDecision::AlreadyLocal
        ));
        // 接收机 poll 侧：读到 A、B 都是同步回声，永不回发
        assert!(matches!(
            rx.poll_decide(&a, None),
            ClipPollDecision::SyncEcho
        ));
        assert!(matches!(
            rx.poll_decide(&b_content, None),
            ClipPollDecision::SyncEcho
        ));
    }

    /// 复测 #7 回归：同一份内容被用户重新复制（内容没变、系统序
    /// 号变了）要判新复制——它就是现在的最新一条；序号没变则仍
    /// 是无变化，不许每拍重发；发送没落实前序号不推进、重试不丢；
    /// 同步回声排在重复制前面（接收机上序号怎么变都不回发）。
    #[test]
    fn 同内容重新复制判新复制() {
        let a = ClipContent::Text("内容A".into());
        let mut g = ClipGuard::default();
        // 原机：复制 A 并发送成功，序号基线立在 10
        assert!(matches!(
            g.poll_decide(&a, Some(10)),
            ClipPollDecision::Send
        ));
        g.note_sent(a.hash());
        g.note_seq(Some(10));
        // 没再复制：序号不变，无变化
        assert!(matches!(
            g.poll_decide(&a, Some(10)),
            ClipPollDecision::Unchanged
        ));
        // 重新复制 A：序号变 11，判新复制；没落实前再读仍判 Send（重试不丢）
        assert!(matches!(
            g.poll_decide(&a, Some(11)),
            ClipPollDecision::Send
        ));
        assert!(matches!(
            g.poll_decide(&a, Some(11)),
            ClipPollDecision::Send
        ));
        g.note_sent(a.hash());
        g.note_seq(Some(11));
        assert!(matches!(
            g.poll_decide(&a, Some(11)),
            ClipPollDecision::Unchanged
        ));
        // 接收机：A 是同步写入的，序号再变也判回声，绝不回发
        let mut rx = ClipGuard::default();
        rx.note_applied(a.clone(), Some(a.hash()));
        assert!(matches!(
            rx.poll_decide(&a, Some(42)),
            ClipPollDecision::SyncEcho
        ));
        assert!(matches!(
            rx.poll_decide(&a, Some(43)),
            ClipPollDecision::SyncEcho
        ));
    }

    /// 手动模式观察登记：推进基线（再读判无变化）但绝不进发送
    /// 登记——没发出去的内容若被对方传回，必须照常判 Proceed。
    #[test]
    fn 手动观察只推进基线不冒充已发送() {
        let mut g = ClipGuard::default();
        let c = ClipContent::Text("手动攒着的内容".into());
        assert!(matches!(g.poll_decide(&c, None), ClipPollDecision::Send));
        g.note_observed(c.hash());
        assert!(matches!(
            g.poll_decide(&c, None),
            ClipPollDecision::Unchanged
        ));
        assert!(g.sent.is_empty(), "没发出去不许进发送登记");
        // 对方若传回同一内容：因基线相同判「本机已有」（不重复写），
        // 但绝不能是 EchoDrop——那是发送登记命中，观察登记不许冒充。
        assert!(matches!(
            g.apply_decide(&c),
            ClipApplyDecision::AlreadyLocal
        ));
    }

    /// 设置回落口径：条数只认 1/2/3/5、方式只认 manual，其余回落
    /// 默认（auto / 1 条）；快捷键空值回落 Ctrl+Alt+V。
    #[test]
    fn 同步设置存取与非法回落() {
        let base = std::env::temp_dir().join(format!("bl-clip-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = Node::new(&base, "测试机").expect("建节点");
        assert_eq!(node.clipboard_sync_mode(), "auto");
        assert_eq!(node.clipboard_sync_count(), 1);
        assert_eq!(node.clipboard_sync_hotkey(), "Ctrl+Alt+V");
        node.set_clipboard_sync_mode("manual").unwrap();
        assert_eq!(node.clipboard_sync_mode(), "manual");
        node.set_clipboard_sync_mode("乱填").unwrap();
        assert_eq!(node.clipboard_sync_mode(), "auto", "非法方式必须回落 auto");
        node.set_clipboard_sync_count(5).unwrap();
        assert_eq!(node.clipboard_sync_count(), 5);
        node.set_clipboard_sync_count(4).unwrap();
        assert_eq!(node.clipboard_sync_count(), 1, "4 不在选项内，必须回落 1");
        node.set_clipboard_sync_hotkey("Ctrl+Shift+P").unwrap();
        assert_eq!(node.clipboard_sync_hotkey(), "Ctrl+Shift+P");
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod v265_tests {
    use super::*;

    /// #13 日期分夹名：UTC+8 口径下与日历一致（含跨年/闰年边界）。
    #[test]
    fn 接收日期目录名正确() {
        assert_eq!(receive_date_dir_name(0), "1970-01-01");
        // 2026-11-22 12:00 UTC = 北京时间 20:00，仍是 22 日
        assert_eq!(receive_date_dir_name(1_795_348_800_000), "2026-11-22");
        // 2026-11-22 17:00 UTC = 北京时间 23 日 01:00，跨日
        assert_eq!(receive_date_dir_name(1_795_366_800_000), "2026-11-23");
        // 2024-02-29 闰日（2024-02-29 12:00 UTC）
        assert_eq!(receive_date_dir_name(1_709_208_000_000), "2024-02-29");
    }

    /// #12 排空语义的地基：loopback 连接 send 后 outbound_lag 必须归零
    /// （直连流 flush 即交付），服务层据此才敢进「校验中」。
    #[tokio::test]
    async fn 环回连接出站滞留归零() {
        let (a, b) = bluelink_transport::loopback_pair();
        a.send(&Frame::Control(Control::Ping)).await.unwrap();
        assert_eq!(a.outbound_lag(), 0);
        assert_eq!(b.recv().await.unwrap(), Frame::Control(Control::Ping));
        assert!(b.last_inbound_byte_ms() > 0, "入站字节活动应被记录");
    }
}
