use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetWindow,
    GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, WS_CHILD, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

// Shell surfaces that are top-level and visible but never a window someone wants to share.
const SHELL_CLASSES: [&str; 5] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "Windows.UI.Core.CoreWindow"];

// What the window list knows about a top-level window, before filtering.
#[derive(Debug, Clone, Default)]
pub struct WindowCandidate {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub visible: bool,
    pub cloaked: bool,
    pub style: u32,
    pub ex_style: u32,
    pub owned: bool,
    pub width: i32,
    pub height: i32,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    // Executable file name, e.g. "notepad.exe"; empty when it cannot be read.
    pub process: String,
}

// The usual Alt+Tab rules: visible, not cloaked (other virtual desktops, suspended UWP frames),
// titled, not a tool window, and either unowned or explicitly marked as an app window.
pub fn shareable(c: &WindowCandidate, own_pid: u32) -> bool {
    let app_window = c.ex_style & WS_EX_APPWINDOW.0 != 0;
    c.visible
        && !c.cloaked
        && !c.title.trim().is_empty()
        && c.style & WS_CHILD.0 == 0
        && c.ex_style & WS_EX_TOOLWINDOW.0 == 0
        && (app_window || c.ex_style & WS_EX_NOACTIVATE.0 == 0)
        && (!c.owned || app_window)
        && c.width > 0
        && c.height > 0
        && c.pid != own_pid
        && !SHELL_CLASSES.contains(&c.class.as_str())
}

pub fn filter_windows(candidates: Vec<WindowCandidate>, own_pid: u32) -> Vec<WindowCandidate> {
    candidates.into_iter().filter(|c| shareable(c, own_pid)).collect()
}

fn wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn describe(hwnd: HWND) -> WindowCandidate {
    // SAFETY: consultas de solo lectura sobre una ventana enumerada por el sistema.
    unsafe {
        let len = GetWindowTextLengthW(hwnd).max(0) as usize;
        let mut title = vec![0u16; len + 1];
        let copied = GetWindowTextW(hwnd, &mut title).max(0) as usize;
        let mut class = [0u16; 128];
        let class_len = GetClassNameW(hwnd, &mut class).max(0) as usize;
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32);
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let owner = GetWindow(hwnd, GW_OWNER).ok().filter(|h| !h.is_invalid());
        let root_owner = GetAncestor(hwnd, GA_ROOTOWNER);
        WindowCandidate {
            hwnd: hwnd.0 as isize,
            title: String::from_utf16_lossy(&title[..copied.min(len)]),
            class: String::from_utf16_lossy(&class[..class_len.min(class.len())]),
            visible: IsWindowVisible(hwnd).as_bool(),
            cloaked: cloaked != 0,
            style: GetWindowLongW(hwnd, GWL_STYLE) as u32,
            ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
            owned: owner.is_some() || (!root_owner.is_invalid() && root_owner != hwnd),
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
            pid,
        }
    }
}

fn process_name(pid: u32) -> String {
    // SAFETY: el identificador del proceso se cierra antes de salir.
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(process);
        if !ok {
            return String::new();
        }
        let path = wide(&buf[..(len as usize).min(buf.len())]);
        path.rsplit(['\\', '/']).next().unwrap_or_default().to_owned()
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` apunta al vector de `list_windows`, vivo durante EnumWindows.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowCandidate>) };
    list.push(describe(hwnd));
    BOOL(1)
}

// Shareable top-level windows in Z order (front to back), excluding this process's own windows.
pub fn list_windows() -> Vec<WindowInfo> {
    let mut candidates: Vec<WindowCandidate> = Vec::new();
    // SAFETY: la devolución de llamada solo escribe en `candidates` mientras dura la llamada.
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM((&raw mut candidates) as isize));
    }
    // SAFETY: sin efectos secundarios.
    let own = unsafe { GetCurrentProcessId() };
    filter_windows(candidates, own)
        .into_iter()
        .map(|c| WindowInfo { hwnd: c.hwnd, title: c.title, process: process_name(c.pid) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(title: &str) -> WindowCandidate {
        WindowCandidate {
            hwnd: 1,
            title: title.to_owned(),
            class: String::from("Notepad"),
            visible: true,
            width: 800,
            height: 600,
            pid: 42,
            ..Default::default()
        }
    }

    #[test]
    fn keeps_ordinary_app_windows_in_order() {
        let list = vec![window("Bloc de notas"), window("Explorador"), window("Navegador")];
        let kept: Vec<String> = filter_windows(list, 7).into_iter().map(|c| c.title).collect();
        assert_eq!(kept, ["Bloc de notas", "Explorador", "Navegador"]);
    }

    #[test]
    fn drops_hidden_untitled_tool_and_cloaked_windows() {
        let hidden = WindowCandidate { visible: false, ..window("oculta") };
        let untitled = window("   ");
        let tool = WindowCandidate { ex_style: WS_EX_TOOLWINDOW.0, ..window("herramienta") };
        let cloaked = WindowCandidate { cloaked: true, ..window("otro escritorio") };
        let empty = WindowCandidate { width: 0, ..window("vacía") };
        let child = WindowCandidate { style: WS_CHILD.0, ..window("hija") };
        let kept = filter_windows(vec![hidden, untitled, tool, cloaked, empty, child, window("buena")], 7);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "buena");
    }

    #[test]
    fn owned_windows_need_the_app_window_style() {
        let dialog = WindowCandidate { owned: true, ..window("Guardar como") };
        let app = WindowCandidate { owned: true, ex_style: WS_EX_APPWINDOW.0, ..window("Ventana propia") };
        let kept = filter_windows(vec![dialog, app], 7);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "Ventana propia");
    }

    #[test]
    fn no_activate_windows_are_skipped_unless_app_windows() {
        let overlay = WindowCandidate { ex_style: WS_EX_NOACTIVATE.0, ..window("superposición") };
        let marked = WindowCandidate { ex_style: WS_EX_NOACTIVATE.0 | WS_EX_APPWINDOW.0, ..window("marcada") };
        let kept = filter_windows(vec![overlay, marked], 7);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "marcada");
    }

    #[test]
    fn shell_surfaces_and_own_windows_are_excluded() {
        let desktop = WindowCandidate { class: String::from("Progman"), ..window("Program Manager") };
        let taskbar = WindowCandidate { class: String::from("Shell_TrayWnd"), ..window("Barra de tareas") };
        let mine = WindowCandidate { pid: 7, ..window("RustPlay") };
        assert!(filter_windows(vec![desktop, taskbar, mine], 7).is_empty());
    }
}
