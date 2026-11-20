//! 文件本体进系统剪贴板（右键文件消息「复制文件」）。
//!
//! Windows：写 CF_HDROP——资源管理器 Ctrl+C 一个文件用的正是这个格式，
//! 粘到任何文件夹/资源管理器窗口出来的就是文件本身（用户 2026-11-20
//! 明确要求，v2.5.1 的「复制路径」回退在 Windows 上作废）。
//! 构造器 build_hdrop_data 是纯字节函数，全平台可单测；真正写剪贴板
//! 只在 Windows 实现。非 Windows 平台返回 UNSUPPORTED 前缀的错误，
//! 前端据此维持原有回退（Web ClipboardItem 尝试 → 路径 + 诚实文案）。

/// 构造 CF_HDROP 数据：DROPFILES 头（20 字节）+ UTF-16LE 路径列表。
/// 头布局：pFiles(u32，文件列表相对偏移=20) / pt(i32 x, i32 y) /
/// fNC(i32) / fWide(i32，=1 表示宽字符)。列表中每条路径以 NUL 结尾，
/// 整表再补一个 NUL（双 NUL 收尾）——这是 DROPFILES 的格式契约，
/// 资源管理器靠它解析出文件列表。
// 非 Windows 目标上 set_file_list 不调用本函数（仅单测用），故放行 dead_code
#[cfg_attr(not(windows), allow(dead_code))]
pub fn build_hdrop_data(paths: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_le_bytes()); // pFiles
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0i32.to_le_bytes()); // fNC = FALSE
    out.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE
    for p in paths {
        for u in p.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // 列表终止符（第二个 NUL）
    out
}

/// 把文件列表写进系统剪贴板（Windows：CF_HDROP 本体；其他平台返回
/// UNSUPPORTED 错误，等待前端走既有回退）。
pub fn set_file_list(paths: &[&str]) -> Result<(), String> {
    #[cfg(windows)]
    {
        set_hdrop_windows(&build_hdrop_data(paths))
    }
    #[cfg(not(windows))]
    {
        let _ = paths;
        Err("UNSUPPORTED: 此平台暂未实现系统文件剪贴板".to_string())
    }
}

/// Windows：OpenClipboard → EmptyClipboard → GlobalAlloc(GMEM_MOVEABLE)
/// → 拷贝 → SetClipboardData(CF_HDROP)。成功后内存所有权归系统，
/// 不得再释放；失败路径上自己释放。
#[cfg(windows)]
fn set_hdrop_windows(data: &[u8]) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    // CF_HDROP 的标准格式值（windows crate 里该常量挂在 System::Ole 下，
    // 为一个常量多开一个 feature 不值当，这里直接用标准值并注明）
    const CF_HDROP_ID: u32 = 15;
    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败：{e}"))?;
        let result = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| format!("清空剪贴板失败：{e}"))?;
            let hg: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, data.len())
                .map_err(|e| format!("分配剪贴板内存失败：{e}"))?;
            let ptr = GlobalLock(hg);
            if ptr.is_null() {
                let _ = GlobalFree(Some(hg));
                return Err("锁定剪贴板内存失败".to_string());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
            let _ = GlobalUnlock(hg);
            match SetClipboardData(CF_HDROP_ID, Some(HANDLE(hg.0))) {
                Ok(_) => Ok(()), // 所有权已归系统，不释放
                Err(e) => {
                    let _ = GlobalFree(Some(hg));
                    Err(format!("写入剪贴板失败：{e}"))
                }
            }
        })();
        let _ = CloseClipboard();
        result
    }
}

/// 解析 CF_HDROP 数据块为路径列表（与 build_hdrop_data 互为逆操作）：
/// DROPFILES 头 20 字节，pFiles 给出列表偏移，fWide=1 时是 UTF-16LE
/// 逐条 NUL 结尾、整表双 NUL 收尾；fWide=0 时是本地编码字节串，同布局。
/// 非 Windows 构建里只有单元测试用它，故放行 dead_code。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn parse_hdrop_data(data: &[u8]) -> Vec<String> {
    if data.len() < 20 {
        return Vec::new();
    }
    let rd32 = |off: usize| u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
    let p_files = rd32(0) as usize;
    let f_wide = rd32(16) != 0;
    if p_files >= data.len() {
        return Vec::new();
    }
    let body = &data[p_files..];
    let mut out = Vec::new();
    if f_wide {
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        for part in units.split(|u| *u == 0) {
            if part.is_empty() {
                break; // 双 NUL 收尾
            }
            out.push(String::from_utf16_lossy(part));
        }
    } else {
        for part in body.split(|b| *b == 0) {
            if part.is_empty() {
                break;
            }
            out.push(String::from_utf8_lossy(part).to_string());
        }
    }
    out
}

/// 读系统剪贴板里的文件列表（粘贴文件用）：资源管理器复制文件后，
/// 剪贴板里就是这个格式。返回 Ok(空) 表示剪贴板里没有文件对象；
/// Err 以 UNSUPPORTED 开头表示此平台暂未实现读取（前端据此给诚实提示）。
#[cfg(windows)]
pub fn get_file_list() -> Result<Vec<String>, String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_HDROP_ID: u32 = 15;
    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败：{e}"))?;
        let result = (|| -> Result<Vec<String>, String> {
            let handle = match GetClipboardData(CF_HDROP_ID) {
                Ok(h) => h,
                Err(_) => return Ok(Vec::new()), // 无此格式 = 没有文件对象
            };
            if handle.is_invalid() {
                return Ok(Vec::new());
            }
            let hg = HGLOBAL(handle.0);
            let size = GlobalSize(Some(hg));
            let ptr = GlobalLock(hg);
            if ptr.is_null() {
                return Err("锁定剪贴板内存失败".to_string());
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            let _ = GlobalUnlock(hg);
            Ok(parse_hdrop_data(&bytes))
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Linux 读取：尽力而为——经 xclip 读 text/uri-list（文件管理器复制文件
/// 时都会提供这个目标）；没有 xclip 时返回 UNSUPPORTED。
#[cfg(target_os = "linux")]
pub fn get_file_list() -> Result<Vec<String>, String> {
    let out = std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "text/uri-list", "-o"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "UNSUPPORTED: 未找到 xclip，无法读取剪贴板文件列表".to_string()
            } else {
                format!("读取剪贴板失败：{e}")
            }
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(p) = parse_file_uri(line) {
            paths.push(p);
        }
    }
    Ok(paths)
}

/// macOS 读取：本轮未实现（NSPasteboard 文件 URL 读取留待后续），
/// 如实返回 UNSUPPORTED，前端给诚实提示、仍可用附件按钮选文件。
#[cfg(target_os = "macos")]
pub fn get_file_list() -> Result<Vec<String>, String> {
    Err("UNSUPPORTED: macOS 暂未支持读取剪贴板文件列表".to_string())
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn get_file_list() -> Result<Vec<String>, String> {
    Err("UNSUPPORTED: 此平台暂未支持读取剪贴板文件列表".to_string())
}

/// file:// URI → 本地路径（百分号解码；只接受本机文件）。
#[cfg(target_os = "linux")]
fn parse_file_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if !host.is_empty() && host != "localhost" {
        return None;
    }
    Some(percent_decode(path))
}

#[cfg(target_os = "linux")]
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::{build_hdrop_data, parse_hdrop_data};

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn hdrop头字段() {
        let d = build_hdrop_data(&["C:\\a.txt"]);
        assert_eq!(u32_at(&d, 0), 20, "pFiles 必须是文件列表偏移 20");
        assert_eq!(u32_at(&d, 4), 0, "pt.x");
        assert_eq!(u32_at(&d, 8), 0, "pt.y");
        assert_eq!(u32_at(&d, 12), 0, "fNC");
        assert_eq!(u32_at(&d, 16), 1, "fWide 必须为 1（宽字符）");
    }

    #[test]
    fn hdrop路径编码与双nul收尾() {
        // "C:\a.txt" 共 8 个字符 → UTF-16LE 16 字节 + 路径 NUL 2 字节
        // + 列表终止 NUL 2 字节；总长 = 20 + 16 + 2 + 2 = 40
        let d = build_hdrop_data(&["C:\\a.txt"]);
        assert_eq!(d.len(), 40);
        // 首字符 'C' 的 UTF-16LE
        assert_eq!(&d[20..22], &[0x43u8, 0x00]);
        // 结尾 4 字节必须是双 NUL（路径终止 + 列表终止）
        assert_eq!(&d[d.len() - 4..], &[0u8, 0, 0, 0]);
        // 路径段解码回原字符串
        let units: Vec<u16> = d[20..36]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "C:\\a.txt");
    }

    #[test]
    fn hdrop多路径与中文路径() {
        let d = build_hdrop_data(&["D:\\甲.txt", "E:\\b.bin"]);
        // 头 20 + (8 字符*2 + 2) + (8 字符*2 + 2) + 终止 2 = 58
        assert_eq!(d.len(), 20 + 18 + 18 + 2);
        assert_eq!(&d[d.len() - 4..], &[0u8, 0, 0, 0]);
        // 中文字符「甲」U+7532 的 UTF-16LE 出现在第一条路径内
        assert!(d.windows(2).any(|w| w == [0x32u8, 0x75]));
    }

    #[test]
    fn hdrop构造解析往返() {
        let paths = ["C:\\a.txt", "D:\\甲.txt", "E:\\sub dir\\b.bin"];
        let d = build_hdrop_data(&paths);
        assert_eq!(parse_hdrop_data(&d), paths);
        assert!(parse_hdrop_data(&[]).is_empty());
        assert!(parse_hdrop_data(&build_hdrop_data(&[])).is_empty());
    }

    #[test]
    fn hdrop空列表只有头与终止符() {
        let d = build_hdrop_data(&[]);
        assert_eq!(d.len(), 22);
        assert_eq!(&d[20..], &[0u8, 0]);
    }
}
