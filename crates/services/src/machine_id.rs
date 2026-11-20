//! 操作系统机器标识读取（设备 ID 机器绑定的锚，见
//! bluelink_domain::machine_bound_device_id）。
//!
//! 只读本机标识、只在本机参与派生计算，原始值绝不外发、绝不落日志。
//! 读不到（权限/平台异常）返回 None，调用方回退随机 ID 并落日志说明。
//! 不用蓝牙 MAC：适配器禁用/更换/首启未就绪都会让结果漂移。

/// Windows：注册表 MachineGuid（HKLM\SOFTWARE\Microsoft\Cryptography）。
#[cfg(windows)]
pub fn read_machine_id() -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"SOFTWARE\Microsoft\Cryptography").ok()?;
    let value: String = key.get_value("MachineGuid").ok()?;
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Linux：/etc/machine-id（dbus 的同值文件作备选）。
#[cfg(target_os = "linux")]
pub fn read_machine_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// macOS：IOPlatformUUID（ioreg 输出里解析）。
#[cfg(target_os = "macos")]
pub fn read_machine_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("IOPlatformUUID") {
            let value = line.split('=').nth(1)?.trim().trim_matches('"').to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 其他平台：无可靠机器标识来源，回退随机 ID。
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn read_machine_id() -> Option<String> {
    None
}
