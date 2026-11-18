//! 极简运行日志：关键事件按天写入数据目录 `logs/bluelink-YYYY-MM-DD.log`，同时输出到 stderr。
//! 应用从桌面图标启动时没有控制台，日志文件是排查蓝牙、传输、AI 问题的唯一依据。
//! 时间统一记 UTC（文件头会注明），避免引入时区依赖。

use std::io::Write;
use std::sync::Mutex;

static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static LOG_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// 初始化日志（Node 创建时调用一次）：打开当天的日志文件追加写。
pub fn init_log(data_dir: &std::path::Path) {
    let dir = data_dir.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let (y, m, d, _, _, _) = now_parts();
    let path = dir.join(format!("bluelink-{y:04}-{m:02}-{d:02}.log"));
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        if let Ok(mut g) = LOG_FILE.lock() {
            *g = Some(f);
        }
        if let Ok(mut g) = LOG_PATH.lock() {
            *g = Some(path);
        }
    }
}

/// 当前日志文件路径（设置页展示与「打开日志目录」用）。
pub fn log_path() -> Option<std::path::PathBuf> {
    LOG_PATH.lock().ok().and_then(|g| g.clone())
}

/// 写一行日志：`[YYYY-MM-DD HH:MM:SS UTC] [tag] 内容`。
pub fn log_line(tag: &str, msg: &str) {
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
