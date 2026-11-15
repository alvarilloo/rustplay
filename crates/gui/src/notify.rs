// Windows notifications: WinRT toasts, or a notification-area balloon when toasts are unavailable.
//
// The MSIX package has its own identity. Unpackaged, RustPlay gives its process an explicit
// AppUserModelID and registers it under HKCU\Software\Classes\AppUserModelId (display name and
// icon), which lets an unpackaged app show toasts without a Start-menu shortcut.

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager, ToastNotifier};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, PostMessageW};
use windows::core::{HSTRING, IInspectable, PCWSTR, w};

// Must match AppUserModelID in packaging/inno/RustPlay.iss.
pub const AUMID: &str = "RustPlay.App";
const AUMID_KEY: &str = r"Software\Classes\AppUserModelId\RustPlay.App";

// Called once at start-up, before any window is created.
pub fn init_identity() {
    // The portable build writes nothing to the registry, so it has no notification identity.
    if crate::autostart::is_packaged() || crate::autostart::is_portable() {
        return;
    }
    unsafe {
        if let Err(e) = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID)) {
            tracing::warn!("no se pudo fijar el AppUserModelID: {e}");
        }
    }
    let key = HSTRING::from(AUMID_KEY);
    set_value(&key, w!("DisplayName"), "RustPlay");
    if let Some(icon) = icon_file() {
        set_value(&key, w!("IconUri"), &icon);
    }
}

fn set_value(key: &HSTRING, name: PCWSTR, value: &str) {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32)
    };
    if result.is_err() {
        tracing::warn!("no se pudo registrar RustPlay para las notificaciones: {result:?}");
    }
}

// The logo as a PNG file in the data folder, which is what the notification centre shows.
fn icon_file() -> Option<String> {
    const LOGO: &[u8] = include_bytes!("../assets/logo.png");
    let path = rustplay_core::identity::data_dir().ok()?.join("notification.png");
    let current = std::fs::metadata(&path).map(|m| m.len()).ok();
    if current != Some(LOGO.len() as u64) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, LOGO).ok()?;
    }
    Some(path.display().to_string())
}

// True when the foreground window belongs to RustPlay (the main window or a mirror window).
pub fn own_window_in_foreground() -> bool {
    unsafe {
        let window = GetForegroundWindow();
        if window.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(window, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
}

fn toast_xml(title: &str, body: &str) -> String {
    let body = if body.is_empty() { String::new() } else { format!("<text>{}</text>", escape(body)) };
    format!(
        "<toast launch=\"open\" activationType=\"foreground\"><visual><binding template=\"ToastGeneric\"><text>{}</text>{body}</binding></visual></toast>",
        escape(title)
    )
}

enum Backend {
    Unknown,
    Toast(ToastNotifier),
    Balloon,
}

pub struct Notifier {
    hwnd: HWND,
    backend: Backend,
}

impl Notifier {
    pub fn new(hwnd: HWND) -> Self {
        // Without a registered identity the portable build uses the notification-area balloon.
        let backend = if crate::autostart::is_portable() { Backend::Balloon } else { Backend::Unknown };
        Self { hwnd, backend }
    }

    fn notifier() -> windows::core::Result<ToastNotifier> {
        if crate::autostart::is_packaged() {
            ToastNotificationManager::CreateToastNotifier()
        } else {
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
        }
    }

    fn toast(&self, notifier: &ToastNotifier, title: &str, body: &str) -> windows::core::Result<()> {
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(toast_xml(title, body)))?;
        let toast = ToastNotification::CreateToastNotification(&xml)?;
        let hwnd = self.hwnd.0 as isize;
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), crate::tray::WM_SHOW_APP, WPARAM(0), LPARAM(0));
            }
            Ok(())
        }))?;
        notifier.Show(&toast)
    }

    // Shows a notification unless one of RustPlay's own windows is in front.
    pub fn show(&mut self, title: &str, body: &str) {
        if own_window_in_foreground() {
            return;
        }
        if matches!(self.backend, Backend::Unknown) {
            self.backend = match Self::notifier() {
                Ok(notifier) => Backend::Toast(notifier),
                Err(e) => {
                    tracing::warn!("no se pueden mostrar notificaciones de Windows, se usará el área de notificación: {e}");
                    Backend::Balloon
                }
            };
        }
        if let Backend::Toast(notifier) = &self.backend {
            match notifier.Setting() {
                Ok(NotificationSetting::Enabled) => {}
                // The user or a policy turned RustPlay's notifications off in Windows.
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!("Windows no reconoce las notificaciones de RustPlay, se usará el área de notificación: {e}");
                    self.backend = Backend::Balloon;
                }
            }
        }
        if let Backend::Toast(notifier) = &self.backend {
            match self.toast(notifier, title, body) {
                Ok(()) => return,
                Err(e) => {
                    tracing::warn!("no se pudo mostrar la notificación, se usará el área de notificación: {e}");
                    self.backend = Backend::Balloon;
                }
            }
        }
        crate::tray::balloon(self.hwnd, title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_escaped() {
        let xml = toast_xml("«A&B» <x>", "");
        assert!(xml.contains("<text>«A&amp;B» &lt;x&gt;</text></binding>"));
        assert!(toast_xml("t", "a\"b").contains("<text>a&quot;b</text>"));
    }
}
