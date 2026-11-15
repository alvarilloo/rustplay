#![windows_subsystem = "windows"]

mod autostart;
mod backend;
mod brands;
mod folder;
mod gfx;
mod history;
mod i18n;
mod links;
mod logbuf;
mod monitors;
mod notify;
mod popup;
mod preview;
mod privacy;
mod settings;
mod svg;
mod theme;
mod tray;
mod ui;
mod syscheck;
mod update;
mod updater;
mod window_icons;

use std::cell::RefCell;
use std::fs::File;
use std::sync::Mutex;

use windows::Win32::Foundation::{POINT, ERROR_ALREADY_EXISTS, GetLastError, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_MAINWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

use crate::gfx::Gfx;
use crate::ui::App;

const TIMER_FRAME: usize = 1;
const TIMER_DISCOVER: usize = 2;
const WIDTH: f32 = 1420.0;
const HEIGHT: f32 = 872.0;
const MIN_WIDTH: f32 = 960.0;
const MIN_HEIGHT: f32 = 560.0;
const CF_UNICODETEXT: u32 = 13;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_NCMOUSELEAVE: u32 = 0x02A2;

struct Window {
    app: RefCell<App>,
    frame_timer: RefCell<Option<u32>>,
    tracking: RefCell<bool>,
    nc_tracking: RefCell<bool>,
    tip: RefCell<String>,
    taskbar_created: u32,
    preview: RefCell<Option<preview::Script>>,
}

fn main() {
    let preview_dir = preview::dir();
    if preview_dir.is_some() {
        preview::enable();
    }
    let background = autostart::launched_at_startup() && preview_dir.is_none();
    // A second launch just brings the running instance to the front.
    if preview_dir.is_none() {
    unsafe {
        // The handle stays open for the life of the process.
        let _mutex = CreateMutexW(None, false, w!(r"Local\RustPlay.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !background && let Ok(existing) = FindWindowW(w!("RustPlayWindow"), None) {
                let _ = PostMessageW(Some(existing), tray::WM_SHOW_APP, WPARAM(0), LPARAM(0));
            }
            return;
        }
    }
    }
    privacy::set_hide_ips(settings::Settings::load().hide_ips);
    // The preview must not truncate the log of a RustPlay that is already running.
    match &preview_dir {
        None => init_logging(None),
        Some(dir) => {
            let _ = std::fs::create_dir_all(dir);
            init_logging(Some(dir.join("preview.log")));
            std::panic::set_hook(Box::new(|info| {
                tracing::error!("pánico: {info}
{}", std::backtrace::Backtrace::force_capture());
            }));
        }
    }
    let forced = if preview_dir.is_some() { std::env::var("RUSTPLAY_PREVIEW_LANG").ok().and_then(|v| v.parse::<usize>().ok()) } else { None };
    i18n::apply(forced.unwrap_or_else(|| settings::Settings::load().language));
    tray::allow_dark_menus();
    if preview_dir.is_none() {
        notify::init_identity();
    }
    tracing::info!("RustPlay {} iniciado{}", env!("CARGO_PKG_VERSION"), if background { " en segundo plano" } else { "" });
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    if let Err(e) = run(background, preview_dir) {
        tracing::error!("RustPlay no pudo iniciarse: {e}");
        let text: Vec<u16> = i18n::tf("error.startup", &[("error", &e)]).encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            MessageBoxW(None, PCWSTR(text.as_ptr()), w!("RustPlay"), MB_ICONERROR);
        }
    }
}

fn init_logging(path: Option<std::path::PathBuf>) {
    let path = match path {
        Some(path) => path,
        None => {
            let Some(dir) = rustplay_core::identity::data_dir().ok() else { return };
            let _ = std::fs::create_dir_all(&dir);
            dir.join("rustplay.log")
        }
    };
    // The previous run's log is kept (as .old.log), so what happened before a restart can still
    // be read.
    let _ = std::fs::rename(&path, path.with_extension("old.log"));
    let Ok(file) = File::create(path) else { return };
    use tracing::Level;
    use tracing_subscriber::filter::{EnvFilter, Targets};
    use tracing_subscriber::prelude::*;
    let filter = std::env::var("RUSTPLAY_LOG").unwrap_or_else(|_| String::from("info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(Mutex::new(file))
        .with_filter(EnvFilter::new(filter));
    let live = Targets::new()
        .with_default(Level::INFO)
        .with_target("rustplay_core", Level::DEBUG)
        .with_target("rustplay_rx", Level::DEBUG)
        .with_target("rustplay_tx", Level::DEBUG)
        .with_target("RustPlay", Level::DEBUG);
    let _ = tracing_subscriber::registry()
        .with(file_layer)
        .with(logbuf::LiveLayer.with_filter(live))
        .try_init();
}

fn run(background: bool, preview_dir: Option<std::path::PathBuf>) -> windows::core::Result<()> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let icon = |size: i32| {
            LoadImageW(Some(instance), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
                .ok()
                .map(|h| HICON(h.0))
        };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hIcon: icon(GetSystemMetrics(SM_CXICON)).unwrap_or_default(),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON)).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: w!("RustPlayWindow"),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            w!("RustPlayWindow"),
            w!("RustPlay"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            None,
        )?;
        let dpi = GetDpiForWindow(hwnd) as f32;
        let scale = dpi / 96.0;
        let saved = settings::Settings::load();
        let theme = theme::Theme::resolve(saved.theme, saved.accent == 0);
        apply_dark_mode(hwnd, theme.dark);
        let backdrop = DWMSBT_MAINWINDOW;
        let mica = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            (&raw const backdrop).cast(),
            size_of_val(&backdrop) as u32,
        )
        .is_ok();
        if mica {
            let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            DwmExtendFrameIntoClientArea(hwnd, &margins)?;
        }
        center(hwnd, (WIDTH * scale) as i32, (HEIGHT * scale) as i32);
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client)?;
        let gfx = Gfx::new(hwnd, client.right as u32, client.bottom as u32, dpi)?;
        let app = App::new(hwnd, gfx, mica && preview_dir.is_none(), background);
        let window = Box::new(Window { app: RefCell::new(app), frame_timer: RefCell::new(None),
            tracking: RefCell::new(false),
            nc_tracking: RefCell::new(false),
            tip: RefCell::new(String::new()),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            preview: RefCell::new(preview_dir.clone().map(preview::Script::new)),
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(window) as isize);
        logbuf::buffer().attach(hwnd);
        // Recomputes the frame so the title bar becomes part of the client area.
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        if preview_dir.is_some() {
            // Off screen and inactive, so a review run never gets in the user's way.
            let _ = SetWindowPos(hwnd, None, -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), preview::TIMER, preview::TICK_MS, None);
        } else {
            SetTimer(Some(hwnd), TIMER_DISCOVER, 30_000, None);
            tray::add(hwnd);
        }
        if !background && preview_dir.is_none() {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn show_main(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

fn quit(hwnd: HWND, window: &Window) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    tray::remove(hwnd);
    if let Ok(mut app) = window.app.try_borrow_mut() {
        app.shutdown();
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

pub(crate) fn apply_dark_mode(hwnd: HWND, dark: bool) {
    let value = BOOL::from(dark);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const value).cast(), size_of::<BOOL>() as u32);
    }
}

unsafe fn center(hwnd: HWND, width: i32, height: i32) {
    unsafe {
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some((&raw mut work).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let width = width.min(work.right - work.left);
        let height = height.min(work.bottom - work.top);
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.top + (work.bottom - work.top - height) / 2;
        let _ = SetWindowPos(hwnd, None, x, y, width, height, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn clipboard_text(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 && len < 4096 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

fn point(lparam: LPARAM, scale: f32) -> (f32, f32) {
    let x = (lparam.0 & 0xFFFF) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
    (x / scale, y / scale)
}

impl Window {
    fn scale(&self, hwnd: HWND) -> f32 {
        unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
    }

    fn redraw(&self, hwnd: HWND) {
        let Ok(mut app) = self.app.try_borrow_mut() else { return };
        app.render();
        let wanted = app.frame_interval();
        drop(app);
        let mut current = self.frame_timer.borrow_mut();
        if *current != wanted {
            unsafe {
                match wanted {
                    Some(ms) => {
                        SetTimer(Some(hwnd), TIMER_FRAME, ms, None);
                    }
                    None => {
                        let _ = KillTimer(Some(hwnd), TIMER_FRAME);
                    }
                }
            }
            *current = wanted;
        }
    }

    fn resize_to_client(&self, hwnd: HWND) {
        let mut client = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut client);
        }
        if let Ok(mut app) = self.app.try_borrow_mut() {
            let dpi = unsafe { GetDpiForWindow(hwnd) } as f32;
            app.resize(client.right as u32, client.bottom as u32, dpi);
        }
    }
}

fn frame_size(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let padded = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        (GetSystemMetricsForDpi(SM_CXFRAME, dpi) + padded, GetSystemMetricsForDpi(SM_CYFRAME, dpi) + padded)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Window;
    // Drops the system title bar but keeps the resize borders on the left, right and bottom.
    if msg == WM_NCCALCSIZE && wparam.0 != 0 && !ptr.is_null() {
        let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
        let (fx, fy) = frame_size(hwnd);
        let rect = &mut params.rgrc[0];
        rect.left += fx;
        rect.right -= fx;
        rect.bottom -= fy;
        if unsafe { IsZoomed(hwnd) }.as_bool() {
            rect.top += fy;
        }
        return LRESULT(0);
    }
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let window = unsafe { &*ptr };
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut ps);
            }
            window.redraw(hwnd);
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let hit = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            if hit.0 as u32 != HTCLIENT {
                return hit;
            }
            let mut pt = POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 };
            unsafe {
                let _ = ScreenToClient(hwnd, &mut pt);
            }
            let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
            let (fx, fy) = frame_size(hwnd);
            if !zoomed && pt.y < fy {
                let mut client = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut client);
                }
                let code = if pt.x < fx * 2 {
                    HTTOPLEFT
                } else if pt.x >= client.right - fx * 2 {
                    HTTOPRIGHT
                } else {
                    HTTOP
                };
                return LRESULT(code as isize);
            }
            let scale = window.scale(hwnd);
            let hit = window
                .app
                .try_borrow()
                .map(|app| app.nc_hit(pt.x as f32 / scale, pt.y as f32 / scale))
                .unwrap_or(ui::Hit::Client);
            LRESULT(match hit {
                ui::Hit::Caption => HTCAPTION,
                ui::Hit::Maximize => HTMAXBUTTON,
                ui::Hit::Client => HTCLIENT,
            } as isize)
        }
        WM_NCMOUSEMOVE => {
            let over_max = wparam.0 as u32 == HTMAXBUTTON;
            if over_max && !*window.nc_tracking.borrow() {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut track);
                }
                *window.nc_tracking.borrow_mut() = true;
            }
            let changed = window
                .app
                .try_borrow_mut()
                .map(|mut app| {
                    app.mouse_leave();
                    app.set_max_hover(over_max)
                })
                .unwrap_or(false);
            if changed {
                window.redraw(hwnd);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_NCMOUSELEAVE => {
            *window.nc_tracking.borrow_mut() = false;
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.set_max_hover(false);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 as u32 == HTMAXBUTTON => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.press_max();
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_NCLBUTTONUP if wparam.0 as u32 == HTMAXBUTTON => {
            if let Ok(mut app) = window.app.try_borrow_mut()
                && app.release_max()
            {
                app.toggle_maximize();
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.set_active((wparam.0 & 0xFFFF) != 0);
            }
            window.redraw(hwnd);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_SHOWWINDOW => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.set_visible(wparam.0 != 0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        logbuf::WM_LOG => {
            logbuf::buffer().acknowledge();
            let visible = window.app.try_borrow_mut().map(|mut app| app.logs_changed()).unwrap_or(false);
            if visible {
                window.redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.set_maximized(wparam.0 as u32 == SIZE_MAXIMIZED);
                app.set_visible(wparam.0 as u32 != SIZE_MINIMIZED);
            }
            window.resize_to_client(hwnd);
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let rect = unsafe { &*(lparam.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            window.resize_to_client(hwnd);
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            let scale = window.scale(hwnd);
            info.ptMinTrackSize.x = (MIN_WIDTH * scale) as i32;
            info.ptMinTrackSize.y = (MIN_HEIGHT * scale) as i32;
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == preview::TIMER {
                let more = match (window.preview.try_borrow_mut(), window.app.try_borrow_mut()) {
                    (Ok(mut script), Ok(mut app)) => script.as_mut().is_some_and(|s| s.tick(hwnd, &mut app)),
                    _ => true,
                };
                if !more {
                    quit(hwnd, window);
                }
                window.redraw(hwnd);
            } else if wparam.0 == TIMER_DISCOVER {
                if let Ok(mut app) = window.app.try_borrow_mut() {
                    app.periodic_refresh();
                    app.update_tick();
                }
            } else {
                window.redraw(hwnd);
            }
            LRESULT(0)
        }
        popup::WM_POPUP => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.on_popups();
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        backend::WM_BACKEND => {
            let mut attention = false;
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.pump();
                attention = app.take_attention();
                let tip = app.tray_tip();
                if *window.tip.borrow() != tip {
                    tray::set_tip(hwnd, &tip);
                    *window.tip.borrow_mut() = tip;
                }
            }
            if attention {
                show_main(hwnd);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if !*window.tracking.borrow() {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut track);
                }
                *window.tracking.borrow_mut() = true;
            }
            let (x, y) = point(lparam, window.scale(hwnd));
            let changed = window.app.try_borrow_mut().map(|mut app| app.mouse_move(x, y)).unwrap_or(false);
            if changed {
                window.redraw(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            *window.tracking.borrow_mut() = false;
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.mouse_leave();
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let (x, y) = point(lparam, window.scale(hwnd));
            unsafe {
                SetCapture(hwnd);
            }
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.mouse_down(x, y);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = point(lparam, window.scale(hwnd));
            unsafe {
                let _ = ReleaseCapture();
            }
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.mouse_up(x, y);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.wheel(delta);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let over_text = (lparam.0 & 0xFFFF) as u32 == HTCLIENT
                && window.app.try_borrow().map(|app| app.over_text_input()).unwrap_or(false);
            if over_text {
                unsafe {
                    if let Ok(cursor) = LoadCursorW(None, IDC_IBEAM) {
                        SetCursor(Some(cursor));
                    }
                }
                LRESULT(1)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u32;
            // SAFETY: reads the keyboard state of this thread's current message.
            let shift = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) } < 0;
            let consumed = window.app.try_borrow_mut().is_ok_and(|mut app| {
                msg == WM_KEYDOWN && app.key_down(vk, shift)
            });
            if consumed {
                window.redraw(hwnd);
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_CHAR => {
            let Some(c) = char::from_u32(wparam.0 as u32) else { return LRESULT(0) };
            if c == '\u{16}' {
                if let Some(text) = clipboard_text(hwnd)
                    && let Ok(mut app) = window.app.try_borrow_mut()
                {
                    app.paste(&text);
                }
            } else if let Ok(mut app) = window.app.try_borrow_mut() {
                app.key_char(c);
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.focus_lost();
            }
            window.redraw(hwnd);
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_DWMCOLORIZATIONCOLORCHANGED | WM_THEMECHANGED => {
            if let Ok(mut app) = window.app.try_borrow_mut() {
                app.refresh_theme();
            }
            window.redraw(hwnd);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // Windows is signing out or an installer asks RustPlay to close (Restart Manager).
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                quit(hwnd, window);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let hide = window.app.try_borrow().map(|app| app.keeps_running_hidden()).unwrap_or(false);
            if hide {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            } else {
                quit(hwnd, window);
            }
            LRESULT(0)
        }
        updater::WM_QUIT_FOR_UPDATE => {
            quit(hwnd, window);
            LRESULT(0)
        }
        tray::WM_SHOW_APP => {
            show_main(hwnd);
            LRESULT(0)
        }
        tray::WM_TRAY => {
            match (lparam.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | windows::Win32::UI::Shell::NIN_BALLOONUSERCLICK => show_main(hwnd),
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let Ok(model) = window.app.try_borrow().map(|app| app.tray_menu()) else { return LRESULT(0) };
                    match tray::menu(hwnd, &model) {
                        Some(tray::CMD_OPEN) => show_main(hwnd),
                        Some(tray::CMD_EXIT) => quit(hwnd, window),
                        Some(command) => {
                            let show = window.app.try_borrow_mut().map(|mut app| app.tray_command(command)).unwrap_or(false);
                            if show {
                                show_main(hwnd);
                            }
                            window.redraw(hwnd);
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        m if m == window.taskbar_created && m != 0 => {
            tray::add(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr.cast_mut()));
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
