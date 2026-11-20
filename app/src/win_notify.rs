//! Windows 原生系统通知（v2.5.2）：WinRT toast + 点击激活回调。
//!
//! 背景：tauri-plugin-notification 在桌面端没有通知点击回调（v2.5.1 如实
//! 记录），故 Windows 改走原生路线，其他平台不编译本模块、仍走官方插件。
//!
//! 机制（免管理员、免改安装器，全是每用户注册表 + 进程内 COM 注册）：
//! - init（setup 最早调用）：写 HKCU 注册表——AUMID 的 DisplayName 与
//!   CustomActivator（指向本模块固定的激活器 CLSID）、CLSID 的
//!   LocalServer32（本 exe 路径）；随后在专用线程里 CoRegisterClassObject
//!   注册进程内类工厂。应用活着时点通知，系统直接回调到本进程；应用
//!   不在时系统按 LocalServer32 拉起 exe，启动流程同样注册，随后收到
//!   Activate（此时应用本就顺势启动，语义自洽）。
//! - send：把 title/body/会话 ID 交给专用线程（MTA），拼 toast XML 并
//!   Show；会话 ID 放在 toast 的 launch 参数里（conversation=<ID>），
//!   激活时原样回到 Activate。
//! - Activate：解析会话 ID → crate::on_notification_activated（窗口回
//!   前台 + emit 事件，前端打开对应会话）。
//!
//! 开关与三态判定不在本模块：调用方 maybe_notify_new_message 已按
//! sys_notify 与窗口前台状态把关，本模块只负责投递与激活。

use std::sync::mpsc;
use std::sync::OnceLock;
use tauri::AppHandle;
use windows::core::{implement, IUnknown, Interface, BOOL, GUID, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::System::Com::{
    CoInitializeEx, CoRegisterClassObject, IClassFactory, IClassFactory_Impl, CLSCTX_LOCAL_SERVER,
    COINIT_MULTITHREADED, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

/// 本应用 AUMID：与 tauri.conf.json 的 identifier 保持一致。
const AUMID: &str = "app.bluelink.desktop";
/// 通知激活器 CLSID（本应用固定专用，注册表与回调靠它对上，勿改）。
const ACTIVATOR_CLSID_STR: &str = "{6CC36E32-ED19-470E-BDA6-30FD76B17FF9}";
const ACTIVATOR_CLSID: GUID = GUID::from_u128(0x6CC36E32_ED19_470E_BDA6_30FD76B17FF9);

struct ToastReq {
    title: String,
    body: String,
    conversation_id: String,
}

static TX: OnceLock<mpsc::Sender<ToastReq>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();

/// setup 最早调用：写注册表、起通知线程（COM 注册在该线程内完成）。
pub fn init(app: AppHandle) {
    let _ = APP.set(app);
    if let Err(e) = register_registry() {
        bluelink_services::log_line("notify", &format!("Windows 通知激活注册失败：{e}"));
    }
    let (tx, rx) = mpsc::channel::<ToastReq>();
    let _ = TX.set(tx);
    let spawned = std::thread::Builder::new()
        .name("win-notify".to_string())
        .spawn(move || notify_thread(rx));
    if let Err(e) = spawned {
        bluelink_services::log_line("notify", &format!("Windows 通知线程启动失败：{e}"));
    }
}

/// 投递一条 toast（title/body/会话口径与插件通道完全一致，由调用方拼好）。
pub fn send(title: &str, body: &str, conversation_id: &str) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(ToastReq {
            title: title.to_string(),
            body: body.to_string(),
            conversation_id: conversation_id.to_string(),
        });
    }
}

/// 每用户注册表：AUMID（DisplayName/CustomActivator）+ CLSID LocalServer32。
/// 幂等：每次启动重写一遍，exe 换路径（升级/改装）后自动指向新路径。
fn register_registry() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (aumid, _) = hkcu
        .create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))
        .map_err(|e| e.to_string())?;
    aumid
        .set_value("DisplayName", &"BlueLink")
        .map_err(|e| e.to_string())?;
    aumid
        .set_value("CustomActivator", &ACTIVATOR_CLSID_STR)
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let (cls, _) = hkcu
        .create_subkey(format!(
            r"Software\Classes\CLSID\{ACTIVATOR_CLSID_STR}\LocalServer32"
        ))
        .map_err(|e| e.to_string())?;
    cls.set_value("", &format!("\"{}\"", exe.display()))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 通知线程：初始化 MTA、注册激活类工厂（进程存活期间有效），然后串行
/// 处理发送请求。toast 的 WinRT 调用都收在这条线程里，不占主线程。
fn notify_thread(rx: mpsc::Receiver<ToastReq>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let factory: IUnknown = NotifyFactory.into();
    // cookie 必须存活到进程结束（撤销注册才用它），本线程与进程同寿，
    // 绑定在栈上即可；COM 侧持有的 factory 引用同理由本线程长驻保活。
    let _cookie = unsafe {
        CoRegisterClassObject(
            &ACTIVATOR_CLSID,
            &factory,
            CLSCTX_LOCAL_SERVER,
            REGCLS_MULTIPLEUSE,
        )
    };
    if let Err(e) = &_cookie {
        bluelink_services::log_line(
            "notify",
            &format!("Windows 通知激活类注册失败：{e}（点通知回窗可能不可用）"),
        );
    }
    while let Ok(req) = rx.recv() {
        if let Err(e) = show_toast(&req) {
            bluelink_services::log_line("notify", &format!("Windows 通知发送失败：{e}"));
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn show_toast(req: &ToastReq) -> Result<(), String> {
    let launch = format!("conversation={}", req.conversation_id);
    let xml_text = format!(
        "<toast launch=\"{}\" activationType=\"foreground\"><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(&launch),
        xml_escape(&req.title),
        xml_escape(&req.body)
    );
    let doc = XmlDocument::new().map_err(|e| e.to_string())?;
    doc.LoadXml(&HSTRING::from(xml_text.as_str()))
        .map_err(|e| e.to_string())?;
    let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
        .map_err(|e| e.to_string())?;
    notifier.Show(&toast).map_err(|e| e.to_string())?;
    Ok(())
}

/// 通知激活回调：系统在用户点通知时调用（应用活着时进本进程；应用不在
/// 时先拉起 exe 再进这里）。只取 launch 里的会话 ID，其余输入不用。
#[implement(INotificationActivationCallback)]
struct NotifyActivator;

impl INotificationActivationCallback_Impl for NotifyActivator_Impl {
    fn Activate(
        &self,
        _app_user_model_id: &PCWSTR,
        invoked_args: &PCWSTR,
        _data: *const NOTIFICATION_USER_INPUT_DATA,
        _count: u32,
    ) -> windows::core::Result<()> {
        let args = unsafe { invoked_args.to_string() }.unwrap_or_default();
        let conv = args.strip_prefix("conversation=").unwrap_or("").to_string();
        if let Some(app) = APP.get() {
            crate::on_notification_activated(app, &conv);
        }
        Ok(())
    }
}

/// 激活器类工厂：COM 据 CLSID 向它要 INotificationActivationCallback。
#[implement(IClassFactory)]
struct NotifyFactory;

impl IClassFactory_Impl for NotifyFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: windows::core::Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut std::ffi::c_void,
    ) -> windows::core::Result<()> {
        if punkouter.is_some() {
            return Err(windows::Win32::Foundation::CLASS_E_NOAGGREGATION.into());
        }
        let cb: INotificationActivationCallback = NotifyActivator.into();
        unsafe { cb.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, _flock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}
