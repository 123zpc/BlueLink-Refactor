//! Win+V 观察钩子（v1.0.4 #4，仅 Windows 实现）。
//!
//! 用户拍板（2026-11-26）：「用户打开剪贴板的同时就是想同步一下」——
//! Win+V 作为手动同步的附加触发键，与可配置快捷键（默认 Ctrl+Alt+V）
//! 并存。Win+V 是系统保留组合，RegisterHotKey 注册不上（微软文档明示
//! Win 组合键保留给系统），故走 Windows 低级键盘钩子 WH_KEYBOARD_LL：
//! **只观察、绝不拦截**——钩子永远放行（CallNextHookEx 原样传递、
//! 返回值不吞键），系统剪贴板历史面板照常弹出，本机同时触发一次
//! 手动同步。
//!
//! 红线：只认 Win+V 这一个组合（Win 键按下期间 V 的首次 keydown），
//! 不记录、不上报任何其他按键；钩子回调里只做原子标记与一次回调
//! 派发，不做任何耗时工作（低级钩子有系统超时，慢了会被摘钩）。
//! 按键自动重复（长按 V 连发 keydown）用 V_DOWN 去重，只触发一次。
//!
//! 范围（用户定）：Windows 特供；macOS 无直达剪贴板历史的系统快捷
//! 键、Linux 无跨桌面环境标准键，其他系统维持可配置快捷键，本模块
//! 在非 Windows 上是空实现（set_active/init 直接返回）。

use std::sync::Mutex;

/// 触发回调（进程级单例，init 时登记一次）：钩子线程只负责调用它，
/// 实际的手动同步派发（异步任务 + 结果事件）在回调内部完成。
static CALLBACK: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// 登记触发回调（启动时调用一次；重复登记以最后一次为准）。
pub fn init(cb: Box<dyn Fn() + Send>) {
    if let Ok(mut g) = CALLBACK.lock() {
        *g = Some(cb);
    }
}

/// 启用/停用观察钩子（与全局快捷键同一生效条件：本机同步开 + 手动
/// 方式）。重复调用幂等：已在目标状态时不重复装/卸、不重复记日志。
pub fn set_active(active: bool) {
    #[cfg(windows)]
    imp::set_active(active);
    #[cfg(not(windows))]
    {
        let _ = active;
    }
}

#[cfg(windows)]
mod imp {
    use super::CALLBACK;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    /// 钩子线程（thread id + JoinHandle，None = 未安装）：set_active
    /// 幂等判定与停用时投递 WM_QUIT 的目标；停用时 join 等线程真正
    /// 卸钩退出后才返回，防快速 停→启 时新旧双钩并存重复触发
    /// （第三方评审采纳）。
    static THREAD_ID: Mutex<Option<(u32, std::thread::JoinHandle<()>)>> = Mutex::new(None);
    /// Win 键当前是否按下（左右 Win 键共用一个标记，够用）。
    static WIN_DOWN: AtomicBool = AtomicBool::new(false);
    /// V 键当前是否处于按下态（长按连发 keydown 去重）。
    static V_DOWN: AtomicBool = AtomicBool::new(false);

    const VK_LWIN: u32 = 0x5B;
    const VK_RWIN: u32 = 0x5C;
    const VK_V: u32 = 0x56;

    /// 低级键盘钩子回调：只观察。识别出「Win 按住 + V 首次按下」就
    /// 派发一次回调，其余一律不理；无论什么情况都原样放行给下一个
    /// 钩子/系统（绝不吞键——吞了 Win+V 面板就弹不出来了）。
    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let ev = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            match ev.vkCode {
                VK_LWIN | VK_RWIN => {
                    if down {
                        WIN_DOWN.store(true, Ordering::SeqCst);
                    } else if up {
                        WIN_DOWN.store(false, Ordering::SeqCst);
                    }
                }
                VK_V => {
                    if down {
                        if WIN_DOWN.load(Ordering::SeqCst) && !V_DOWN.swap(true, Ordering::SeqCst) {
                            // try_lock：钩子回调绝不阻塞等锁（低级钩子有
                            // 系统超时、慢了会被摘钩）；拿不到就跳过这
                            // 一次触发，宁可漏也不卡（第三方评审采纳）
                            if let Ok(g) = CALLBACK.try_lock() {
                                if let Some(f) = &*g {
                                    f();
                                }
                            }
                        }
                    } else if up {
                        V_DOWN.store(false, Ordering::SeqCst);
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    pub fn set_active(active: bool) {
        // 锁中毒（钩子线程 panic 过）不许连累命令层：取回内层继续用
        let mut slot = THREAD_ID.lock().unwrap_or_else(|e| e.into_inner());
        if active == slot.is_some() {
            return; // 已在目标状态：幂等，不重复装卸、不重复记日志
        }
        if !active {
            let (tid, handle) = slot.take().unwrap();
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            // 等钩子线程卸钩退出再返回（WM_QUIT 后 GetMessage 立即
            // 返回，join 很快）；join 在锁内做——hook_proc 不碰这把锁
            let _ = handle.join();
            bluelink_services::log_line("clipboard", "Win+V 同步触发已停用");
            return;
        }
        // 安装：钩子必须装在带消息循环的线程上，单开一条线程跑
        // GetMessage 循环；装钩结果经 channel 回报给本线程再落账。
        let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let spawned = std::thread::Builder::new()
            .name("win-v-hotkey".into())
            .spawn(move || unsafe {
                let hook =
                    match GetModuleHandleW(None)
                        .map_err(|e| e.to_string())
                        .and_then(|hmod| {
                            SetWindowsHookExW(
                                WH_KEYBOARD_LL,
                                Some(hook_proc),
                                Some(windows::Win32::Foundation::HINSTANCE(hmod.0)),
                                0,
                            )
                            .map_err(|e| e.to_string())
                        }) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                let _ = tx.send(Ok(GetCurrentThreadId()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                let _ = UnhookWindowsHookEx(hook);
            });
        let (result, handle) = match spawned {
            Err(e) => (Err(e.to_string()), None),
            Ok(h) => {
                let r = match rx.recv() {
                    Ok(r) => r,
                    Err(e) => Err(format!("钩子线程无回应（{e}）")),
                };
                (r, Some(h))
            }
        };
        match result {
            Ok(tid) => {
                *slot = Some((tid, handle.expect("装钩成功路径必有线程句柄")));
                bluelink_services::log_line(
                    "clipboard",
                    "Win+V 同步触发已启用（只观察不拦截，剪贴板历史面板照常弹出）",
                );
            }
            Err(e) => {
                bluelink_services::log_line(
                    "clipboard",
                    &format!("Win+V 同步触发启用失败：{e}（原有快捷键与按钮不受影响）"),
                );
            }
        }
    }
}
