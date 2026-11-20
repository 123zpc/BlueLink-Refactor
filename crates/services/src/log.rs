//! 极简运行日志：关键事件按天写入日志目录 `bluelink-YYYY-MM-DD.log`，同时输出到 stderr。
//! 应用从桌面图标启动时没有控制台，日志文件是排查蓝牙、传输、AI 问题的唯一依据。
//! 时间统一记 UTC（文件头会注明），避免引入时区依赖。
//!
//! 分级（2026-11-20 v2.5.1 用户定）：两级即可——
//! - essential（log_line）：两种模式都记：启动/退出、连接建立/断开结果、
//!   一切错误与失败（「连接失败于步骤 N/6」这类定案行必须在列）；
//! - verbose（log_verbose）：仅开发者模式记：SCANPROBE 各节、连接分步流水、
//!   剪贴板同步细节等成功路径的诊断细节。
//!
//! 切换开发者模式即时生效（set_verbose），不需重启。
//!
//! 目录与保留：默认 <数据目录>/logs，可经 set_log_dir 改（设置键 log_dir 持久化，
//! 旧目录文件不搬不删）；目录内只保留当天一个文件——启动、切换目录、跨零点
//! 写日志时都会清掉非当天的 bluelink-*.log（只按本应用文件名前缀匹配）。

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static LOG_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static LOG_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// 设置全量（verbose）日志开关：开发者模式开 = 全量，关 = 只记必要日志。
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// 当前是否全量日志。
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// 当天日志文件名。
fn today_file_name() -> String {
    let (y, m, d, _, _, _) = now_parts();
    format!("bluelink-{y:04}-{m:02}-{d:02}.log")
}

/// 清掉目录内非当天的本应用日志文件（只匹配 bluelink-*.log 前缀，
/// 绝不动用户放在同目录的其他文件）。
fn cleanup_old_logs(dir: &std::path::Path) {
    let today = today_file_name();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("bluelink-") && name.ends_with(".log") && name != today {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// 在指定目录打开当天日志（追加写）并设为当前写入目标，同时清理旧日志。
fn open_in_dir(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    cleanup_old_logs(dir);
    let path = dir.join(today_file_name());
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        if let Ok(mut g) = LOG_FILE.lock() {
            *g = Some(f);
        }
        if let Ok(mut g) = LOG_PATH.lock() {
            *g = Some(path);
        }
        if let Ok(mut g) = LOG_DIR.lock() {
            *g = Some(dir.to_path_buf());
        }
        true
    } else {
        false
    }
}

/// 初始化日志（Node 创建时调用一次）：默认目录 <数据目录>/logs。
pub fn init_log(data_dir: &std::path::Path) {
    open_in_dir(&data_dir.join("logs"));
}

/// 切换日志目录（设置页改 log_dir 时调用）：新日志从此写到新目录的当天文件，
/// 旧目录文件不搬不删。返回生效的日志文件路径；失败返回 None 且保持原目录。
pub fn set_log_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if open_in_dir(dir) {
        log_path()
    } else {
        None
    }
}

/// 当前日志目录。
pub fn log_dir() -> Option<std::path::PathBuf> {
    LOG_DIR.lock().ok().and_then(|g| g.clone())
}

/// 当前日志文件路径（设置页展示与「打开日志目录」用）。
pub fn log_path() -> Option<std::path::PathBuf> {
    LOG_PATH.lock().ok().and_then(|g| g.clone())
}

/// 跨零点处理：进程还活着时，第一条新一天的日志触发切换到新当天文件
/// 并清掉昨天的（实现「每天覆盖」，目录里永远只有当天一个文件）。
fn rollover_if_needed() {
    let dir = match log_dir() {
        Some(d) => d,
        None => return,
    };
    let expected = dir.join(today_file_name());
    if log_path().as_deref() != Some(expected.as_path()) {
        open_in_dir(&dir);
    }
}

static PANIC_HOOK_ONCE: std::sync::Once = std::sync::Once::new();

/// 安装全局 panic 钩子（只装一次）：任何线程 panic 时，先把位置、消息、线程名
/// 以 [panic] 标签同步写进当天运行日志（log_line 本身每行 flush 落盘），
/// 再走原有默认钩子（stderr 输出不变）。
///
/// 动机（2026-11-20）：第三方蓝牙库的 FFI 回调里若 panic，panic 穿越 FFI 边界
/// 会直接 abort 进程——界面无提示、日志无痕迹，真机只表现为「闪退」。钩子在
/// panic 开始展开前就会被调用，所以哪怕随后进程 abort，这行「遗书」也已落盘，
/// 下次回传日志即可按 file:line 定位。
pub fn install_panic_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "（无消息）".to_string()
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "（无位置）".to_string());
            let thread = std::thread::current();
            let thread_name = thread.name().unwrap_or("（未命名线程）");
            log_line(
                "panic",
                &format!("线程 {thread_name} panic：{payload}（位置 {location}）"),
            );
            previous(info);
        }));
    });
}

/// 写一行必要日志（两种模式都记）：`[YYYY-MM-DD HH:MM:SS UTC] [tag] 内容`。
pub fn log_line(tag: &str, msg: &str) {
    rollover_if_needed();
    let (y, mo, d, h, mi, s) = now_parts();
    let line = format!("[{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC] [{tag}] {msg}");
    eprintln!("{line}");
    if let Ok(mut g) = LOG_FILE.lock() {
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

/// 写一行全量日志：仅开发者模式（verbose 开）时落盘，普通模式直接丢弃。
/// 成功路径的分步流水、SCANPROBE、剪贴板细节走这里；失败与错误永远走 log_line。
pub fn log_verbose(tag: &str, msg: &str) {
    if verbose() {
        log_line(tag, msg);
    }
}

/// 当前 UTC 时间分量（年月日时分秒），由 Unix 时间戳换算（Howard Hinnant 民用历算法）。
fn now_parts() -> (i64, u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, h, mi, s)
}
