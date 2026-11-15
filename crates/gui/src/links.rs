use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

// Project and donation pages shown on the «Acerca de» page.
pub const GITHUB: &str = "https://github.com/alvarilloo/rustplay";
pub const KOFI: &str = "https://ko-fi.com/alvarilloo";
pub const PAYPAL: &str = "https://paypal.me/alvarilloo";

pub fn open(url: &str) {
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}
