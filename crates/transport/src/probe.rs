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
