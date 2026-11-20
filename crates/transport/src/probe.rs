//! SCANPROBE 扫描诊断日志（仅日志，不影响任何扫描行为与结果）。
//!
//! 动机：经典蓝牙扫描能拿到的信息分散在三套接口里——Win32 经典 Inquiry 的
//! BLUETOOTH_DEVICE_INFO、WinRT AEP 属性表、BLE 广播包——产品界面只用其中
//! 少数几个字段（如经典的 RSSI 就无处可取，见 classic 模块的 AEP 探针注释）。
//! 为了一次性摸清真机上到底能拿到什么，扫描时把三套接口的全部字段逐设备
//! 分段攒成日志行（统一以「SCANPROBE」开头，逐设备有开始/结束标记行），
//! 由上层（app 的扫描轮询）取走写进运行日志，用户回传日志后可一次性解析。
//! 本模块只是线程安全的行队列；各 dump 的触发与节流在各自模块内。

use std::sync::Mutex;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 攒一行 SCANPROBE 日志（各 dump 路径调用）。
pub(crate) fn push_line(line: String) {
    if let Ok(mut g) = LINES.lock() {
        g.push(line);
    }
}

/// 取走攒好的全部 SCANPROBE 日志行（上层负责写进运行日志）。
pub fn take_probe_lines() -> Vec<String> {
    LINES
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

/// 传输层诊断日志出口：由应用在启动时注入（接上 services 的运行日志）。
/// 连接分步日志这类即时性强的诊断走这里，时间戳就是步骤发生的时刻；
/// SCANPROBE 的攒行队列是另一条通道（扫描轮询时批量落盘）。
pub type LogSink = fn(&str, &str);

static SINK: Mutex<Option<LogSink>> = Mutex::new(None);

/// 注入日志出口（应用启动时调用一次）。
pub fn set_log_sink(f: LogSink) {
    if let Ok(mut g) = SINK.lock() {
        *g = Some(f);
    }
}

/// 写一行传输层诊断日志（未注入出口时静默丢弃，不影响功能）。
/// 这一路是 essential：失败与错误定案行（如「连接失败于步骤 N/6」）走这里，
/// 两种日志模式都必须落盘。
pub(crate) fn log(tag: &str, msg: &str) {
    if let Ok(g) = SINK.lock() {
        if let Some(f) = *g {
            f(tag, msg);
        }
    }
}

static STEP_SINK: Mutex<Option<LogSink>> = Mutex::new(None);

/// 注入分步流水日志出口（应用启动时调用一次，接 services 的 log_verbose）。
pub fn set_step_sink(f: LogSink) {
    if let Ok(mut g) = STEP_SINK.lock() {
        *g = Some(f);
    }
}

/// 写一行分步流水日志（verbose）：成功路径的连接步骤、服务端动作顺序等
/// 诊断细节走这里——只在开发者模式（全量日志）下落盘，普通模式丢弃。
/// 失败行不许走这里，走 log()。
pub(crate) fn log_step(tag: &str, msg: &str) {
    if let Ok(g) = STEP_SINK.lock() {
        if let Some(f) = *g {
            f(tag, msg);
        }
    }
}
