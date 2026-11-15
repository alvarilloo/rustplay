use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_numerics::{Matrix3x2, Vector2};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_BORDER_MODE_HARD, D2D1_COLOR_F, D2D1_COMPOSITE_MODE_SOURCE_OVER,
    D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea, DwmGetColorizationColor,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Interface, PCWSTR, w};

use super::DecodedFrame;
use super::cinema::{self, Shortcut, ToastKind};
use super::overlay::{self, Action};
use crate::media::gpu::GpuDevice;
use crate::{Error, Result};

const INPUT_VIEW_CACHE: usize = 16;
const VK_RETURN: usize = 0x0D;
const WM_APP_QUIT: u32 = WM_APP + 1;
const WM_APP_FIT: u32 = WM_APP + 2;
const WM_APP_FULLSCREEN: u32 = WM_APP + 3;
const WM_APP_TOPMOST: u32 = WM_APP + 4;
const WM_APP_BORDERLESS: u32 = WM_APP + 5;
const WM_NCMOUSELEAVE: u32 = 0x02A2;
// Width in device pixels at 96 DPI of the sizing edges of the borderless window.
const RESIZE_BORDER: f32 = 8.0;
const TIMER_HIDE: usize = 1;
const FIT_FRACTION: f64 = 0.9;
const NO_POINTER: u64 = u64::MAX;
const WM_MOUSELEAVE: u32 = 0x02A3;
const IDLE_INTERVAL: Duration = Duration::from_millis(50);
const ANIMATION_INTERVAL: Duration = Duration::from_millis(16);

struct Shared {
    closed: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
    video_width: AtomicU32,
    video_height: AtomicU32,
    fullscreen: AtomicBool,
    controls: AtomicBool,
    open_app: AtomicBool,
    // Record and screenshot buttons are shown.
    capture: AtomicBool,
    // A recording is running (the record button shows the time).
    recording: AtomicBool,
    // GetTickCount64 time until which the controls stay visible.
    visible_until: AtomicU64,
    // Client coordinates of the pointer as (x << 32 | y), or NO_POINTER outside the window.
    pointer: AtomicU64,
    // Index + 1 of the button being pressed.
    pressed: AtomicU32,
    // Something drawn by the overlay changed (set by the window thread).
    dirty: AtomicBool,
    open_requested: AtomicBool,
    record_requested: AtomicBool,
    screenshot_requested: AtomicBool,
    mute_requested: AtomicBool,
    // The audio of this device is muted (kept up to date by the host).
    muted: AtomicBool,
    // Cinema mode: no title bar and frame.
    borderless: AtomicBool,
    // The picture is blurred behind the bars around it.
    blur: AtomicBool,
    // Moving and resizing are disabled.
    locked: AtomicBool,
    stats: AtomicBool,
    topmost: AtomicBool,
    // The toast being shown (`cinema::encode_toast`, 0 = none) and when it started.
    toast: AtomicU32,
    toast_since: AtomicU64,
}

fn now_ms() -> u64 {
    // SAFETY: sin parámetros.
    unsafe { GetTickCount64() }
}

fn dpi_scale(hwnd: HWND) -> f32 {
    // SAFETY: consulta sobre una ventana propia.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
}

impl Shared {
    fn pointer(&self) -> Option<(f32, f32)> {
        let p = self.pointer.load(Ordering::Acquire);
        (p != NO_POINTER).then_some(((p >> 32) as u32 as i32 as f32, p as u32 as i32 as f32))
    }

    fn actions(&self) -> &'static [Action] {
        overlay::actions(self.open_app.load(Ordering::Acquire), self.capture.load(Ordering::Acquire))
    }

    fn layout(&self, scale: f32) -> overlay::Layout {
        let (w, h) = (self.width.load(Ordering::Acquire), self.height.load(Ordering::Acquire));
        overlay::layout(w as f32, h as f32, scale, self.actions(), self.recording.load(Ordering::Acquire))
    }

    fn hovered(&self, scale: f32) -> Option<usize> {
        if !self.controls.load(Ordering::Acquire) {
            return None;
        }
        let (x, y) = self.pointer()?;
        self.layout(scale).hit(x, y)
    }

    fn overlay_wanted(&self, scale: f32) -> bool {
        if !self.controls.load(Ordering::Acquire) {
            return false;
        }
        now_ms() < self.visible_until.load(Ordering::Acquire)
            || self.pressed.load(Ordering::Acquire) != 0
            || self.pointer().is_some_and(|(x, y)| self.layout(scale).over_bar(x, y))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scaling {
    // Whole image with black bars.
    #[default]
    Fit,
    // Fills the window, cropping the edges.
    Fill,
    // Fills the window, distorting the image.
    Stretch,
}

// Already translated tooltips of the hover controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayTexts {
    pub fullscreen: String,
    pub exit_fullscreen: String,
    pub open_app: String,
    pub stop: String,
    pub record: String,
    pub stop_recording: String,
    pub screenshot: String,
    // Names of the actions of the keyboard shortcuts shown in toasts, and the words for their state.
    pub toast_fullscreen: String,
    pub toast_recording: String,
    pub toast_screenshot: String,
    pub toast_mute: String,
    pub toast_blur: String,
    pub toast_cinema: String,
    pub toast_lock: String,
    pub toast_stats: String,
    pub toast_topmost: String,
    pub toast_on: String,
    pub toast_off: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PresenterOptions {
    pub fullscreen: bool,
    pub always_on_top: bool,
    pub scaling: Scaling,
    // Waits for the display refresh (no tearing, slightly more latency).
    pub vsync: bool,
    pub high_quality_scaling: bool,
    // Controls shown while the pointer moves over the window.
    pub controls: bool,
    // Adds the "open the app" button (the host learns it through `take_open_request`).
    pub open_app_button: bool,
    // Adds the record and screenshot buttons (the host learns about presses through
    // `take_record_request` and `take_screenshot_request`).
    pub capture_buttons: bool,
    // Shows `stats_text` in a corner; "{width}", "{height}" and "{fps}" are filled in.
    pub stats: bool,
    pub stats_text: String,
    // No title bar or frame (still resizable by the edges, draggable by the picture).
    pub borderless: bool,
    // Blurred copy of the picture behind the bars around it.
    pub blur_background: bool,
    // Disables moving and resizing the window.
    pub lock_position: bool,
    // Already translated texts of the "screen off" backdrop.
    pub lock_title: String,
    pub lock_message: String,
    pub overlay: OverlayTexts,
}

struct WndState {
    shared: Arc<Shared>,
    fullscreen: bool,
    fit_pending: bool,
    tracking: bool,
    borderless: bool,
    style: u32,
    rect: RECT,
    // The window stays hidden until the first picture tells the device's resolution, so it
    // never appears at a placeholder size and then jumps.
    shown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayKey {
    alpha: f32,
    hovered: Option<usize>,
    pressed: bool,
    fullscreen: bool,
    fps: Option<u32>,
    // Whole seconds recorded; only tracked while the controls are visible.
    recording: Option<u64>,
    toast: Option<(u32, f32)>,
}

struct CachedView {
    texture: ID3D11Texture2D,
    slice: u32,
    view: ID3D11VideoProcessorInputView,
}

struct Target {
    view: Option<ID3D11VideoProcessorOutputView>,
    rtv: ID3D11RenderTargetView,
}

pub struct Presenter {
    pub gpu: Arc<GpuDevice>,
    shared: Arc<Shared>,
    hwnd: HWND,
    ui: Option<JoinHandle<()>>,
    swap: IDXGISwapChain2,
    flags: u32,
    tearing: bool,
    waitable: HANDLE,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)>,
    vp_in: (u32, u32),
    // The video processor must be rebuilt (scaling quality changed) on the next frame.
    vp_dirty: bool,
    cfg: (u32, u32, u32, u32, u32),
    cache: [Option<CachedView>; INPUT_VIEW_CACHE],
    next_slot: usize,
    buf_size: (u32, u32),
    target: Option<Target>,
    options: PresenterOptions,
    last: Option<LastFrame>,
    backdrop: Option<Backdrop>,
    frozen: Option<Frozen>,
    paused: Option<(u32, u32)>,
    // The frame-latency object was already waited on for the next Present.
    slot_ready: bool,
    overlay_alpha: f32,
    overlay_clock: Instant,
    recording_since: Option<Instant>,
    drawn: Option<OverlayKey>,
    frames: u32,
    frames_since: Instant,
    fps: u32,
    // Blurred low-resolution copy of the picture shown in the bars around it.
    ambient: Option<Ambient>,
    ambient_failed: bool,
    ambient_frames: u32,
    // D2D view of the current back buffer, kept between frames.
    d2d_target: Option<ID2D1Bitmap1>,
    // The next presented picture is also copied (with the overlay) for `take_window_capture`.
    grab: bool,
    grabbed: Option<crate::media::screenshot::Image>,
}

struct Ambient {
    size: (u32, u32),
    // Texture the video processor renders the cropped picture into.
    _texture: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    _bitmap: ID2D1Bitmap1,
    image: ID2D1Image,
    dim: ID2D1SolidColorBrush,
    // Holds a picture to show.
    filled: bool,
}

#[derive(Clone)]
struct LastFrame {
    view: ID3D11VideoProcessorInputView,
    size: (u32, u32),
}

struct Backdrop {
    dc: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    fonts: Option<overlay::Fonts>,
}

struct Frozen {
    _texture: ID3D11Texture2D,
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
}

// SAFETY: el dispositivo D3D11 está protegido para multihilo y el Presenter solo se usa
// desde un hilo a la vez (`&mut self`); el HWND y el HANDLE son valores opacos.
unsafe impl Send for Presenter {}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: el puntero de usuario lo fija WM_NCCREATE y se libera en WM_NCDESTROY.
    unsafe {
        if msg == WM_NCCREATE {
            let cs = lp.0 as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let st = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WndState;
        if st.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let state = &mut *st;
        match msg {
            WM_SIZE => {
                if wp.0 != SIZE_MINIMIZED as usize {
                    let l = lp.0 as usize;
                    state
                        .shared
                        .width
                        .store((l & 0xFFFF) as u32, Ordering::Release);
                    state
                        .shared
                        .height
                        .store(((l >> 16) & 0xFFFF) as u32, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                pointer_moved(hwnd, state, lp.0 as u32 as i16 as i32, (lp.0 as u32 >> 16) as i16 as i32);
                LRESULT(0)
            }
            // The picture of a borderless window is its title bar, so its mouse moves are non-client.
            WM_NCMOUSEMOVE => {
                let mut p = POINT { x: lp.0 as u32 as i16 as i32, y: (lp.0 as u32 >> 16) as i16 as i32 };
                if ScreenToClient(hwnd, &mut p).as_bool() {
                    pointer_moved(hwnd, state, p.x, p.y);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_MOUSELEAVE | WM_NCMOUSELEAVE => {
                state.tracking = false;
                let mut at = POINT::default();
                let mut window = RECT::default();
                let inside = GetCursorPos(&mut at).is_ok()
                    && GetWindowRect(hwnd, &mut window).is_ok()
                    && state.borderless
                    && (window.left..window.right).contains(&at.x)
                    && (window.top..window.bottom).contains(&at.y);
                // Moving between the client and non-client parts of a borderless window also leaves.
                if !inside {
                    state.shared.pointer.store(NO_POINTER, Ordering::Release);
                    state.shared.visible_until.store(0, Ordering::Release);
                    state.shared.dirty.store(true, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_NCCALCSIZE if state.borderless && wp.0 != 0 => {
                // The whole window is client area; a maximised one loses the sizing frame's overhang.
                if IsZoomed(hwnd).as_bool() {
                    let params = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let edge = GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                    let rect = &mut params.rgrc[0];
                    rect.left += edge;
                    rect.right -= edge;
                    rect.top += edge;
                    rect.bottom -= edge;
                }
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let default = DefWindowProcW(hwnd, msg, wp, lp);
                LRESULT(nc_hit_test(hwnd, state, lp, default.0 as u32) as isize)
            }
            WM_NCLBUTTONDBLCLK if wp.0 as u32 == HTCAPTION && !state.shared.locked.load(Ordering::Acquire) => {
                toggle_fullscreen(hwnd, state);
                LRESULT(0)
            }
            WM_SYSCOMMAND if state.shared.locked.load(Ordering::Acquire) && matches!(wp.0 & 0xFFF0, 0xF000 | 0xF010 | 0xF030) => {
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_HIDE => {
                let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                state.shared.dirty.store(true, Ordering::Release);
                if state.fullscreen && !state.shared.overlay_wanted(dpi_scale(hwnd)) {
                    SetCursor(None);
                }
                LRESULT(0)
            }
            WM_SETCURSOR
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT
                    && state.fullscreen
                    && !state.shared.overlay_wanted(dpi_scale(hwnd)) =>
            {
                SetCursor(None);
                LRESULT(1)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let scale = dpi_scale(hwnd);
                let visible = state.shared.overlay_wanted(scale);
                match state.shared.hovered(scale) {
                    Some(i) if visible => {
                        state.shared.pressed.store(i as u32 + 1, Ordering::Release);
                        state.shared.dirty.store(true, Ordering::Release);
                        SetCapture(hwnd);
                    }
                    None if msg == WM_LBUTTONDBLCLK
                        && !state.shared.pointer().is_some_and(|(x, y)| state.shared.layout(scale).over_bar(x, y)) =>
                    {
                        toggle_fullscreen(hwnd, state);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let pressed = state.shared.pressed.swap(0, Ordering::AcqRel);
                if pressed != 0 {
                    let _ = ReleaseCapture();
                    state.shared.dirty.store(true, Ordering::Release);
                    let index = pressed as usize - 1;
                    if state.shared.hovered(dpi_scale(hwnd)) == Some(index) {
                        match state.shared.actions().get(index) {
                            Some(Action::Fullscreen) => toggle_fullscreen(hwnd, state),
                            Some(Action::OpenApp) => state.shared.open_requested.store(true, Ordering::Release),
                            Some(Action::Record) => state.shared.record_requested.store(true, Ordering::Release),
                            Some(Action::Screenshot) => state.shared.screenshot_requested.store(true, Ordering::Release),
                            Some(Action::Stop) => {
                                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                            }
                            None => {}
                        }
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN if lp.0 & (1 << 30) == 0 => {
                let held = |vk: i32| GetKeyState(vk) < 0;
                let modified = held(0x11) || held(0x12);
                if let Some(shortcut) = cinema::shortcut_for_key(wp.0, state.fullscreen, modified) {
                    run_shortcut(hwnd, state, shortcut);
                }
                LRESULT(0)
            }
            WM_SYSKEYDOWN if wp.0 == VK_RETURN && lp.0 & (1 << 30) == 0 => {
                toggle_fullscreen(hwnd, state);
                LRESULT(0)
            }
            WM_APP_FIT => {
                let first = !state.shown;
                if state.fullscreen {
                    state.fit_pending = true;
                } else {
                    fit_to_video(hwnd, state, first);
                }
                if first {
                    state.shown = true;
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(hwnd);
                    state.shared.dirty.store(true, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_APP_FULLSCREEN => {
                if (wp.0 != 0) != state.fullscreen {
                    toggle_fullscreen(hwnd, state);
                }
                LRESULT(0)
            }
            WM_APP_TOPMOST => {
                set_topmost(hwnd, state, wp.0 != 0);
                LRESULT(0)
            }
            WM_APP_BORDERLESS => {
                set_borderless(hwnd, state, wp.0 != 0);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                state.shared.closed.store(true, Ordering::Release);
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_APP_QUIT => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                state.shared.closed.store(true, Ordering::Release);
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(st));
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// The pointer moved to (x, y) in client coordinates: shows the controls and keeps tracking it.
unsafe fn pointer_moved(hwnd: HWND, state: &mut WndState, x: i32, y: i32) {
    let packed = ((x as u32 as u64) << 32) | u64::from(y as u32);
    if state.shared.pointer.swap(packed, Ordering::AcqRel) != packed {
        // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo de interfaz.
        unsafe {
            if !state.tracking {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: if state.borderless { TME_LEAVE | TME_NONCLIENT } else { TME_LEAVE },
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                state.tracking = TrackMouseEvent(&mut tme).is_ok();
            }
            state.shared.visible_until.store(now_ms() + overlay::HIDE_AFTER_MS, Ordering::Release);
            state.shared.dirty.store(true, Ordering::Release);
            SetTimer(Some(hwnd), TIMER_HIDE, (overlay::HIDE_AFTER_MS + 50) as u32, None);
            SetCursor(LoadCursorW(None, IDC_ARROW).ok());
        }
    }
}

// Hit testing: a borderless window resizes by its edges and moves by its picture (except over
// the controls); a locked one does neither. `default` is what Windows decided.
unsafe fn nc_hit_test(hwnd: HWND, state: &WndState, lp: LPARAM, default: u32) -> u32 {
    let locked = state.shared.locked.load(Ordering::Acquire);
    if !state.borderless || state.fullscreen {
        return if locked && !state.fullscreen { cinema::locked_hit(default) } else { default };
    }
    if locked {
        return HTCLIENT;
    }
    // SAFETY: consultas Win32 sobre la ventana propia.
    unsafe {
        let mut window = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window);
        let (x, y) = (lp.0 as u32 as i16 as i32, (lp.0 as u32 >> 16) as i16 as i32);
        let scale = dpi_scale(hwnd);
        let border = (RESIZE_BORDER * scale).round() as i32;
        let (w, h) = (window.right - window.left, window.bottom - window.top);
        match cinema::zone(x - window.left, y - window.top, w, h, border) {
            cinema::Zone::Inside => {
                let over_controls = state.shared.overlay_wanted(scale)
                    && state.shared.layout(scale).over_bar((x - window.left) as f32, (y - window.top) as f32);
                if over_controls { HTCLIENT } else { HTCAPTION }
            }
            cinema::Zone::Left => HTLEFT,
            cinema::Zone::Right => HTRIGHT,
            cinema::Zone::Top => HTTOP,
            cinema::Zone::Bottom => HTBOTTOM,
            cinema::Zone::TopLeft => HTTOPLEFT,
            cinema::Zone::TopRight => HTTOPRIGHT,
            cinema::Zone::BottomLeft => HTBOTTOMLEFT,
            cinema::Zone::BottomRight => HTBOTTOMRIGHT,
        }
    }
}

fn show_toast(shared: &Shared, kind: ToastKind, state: Option<bool>) {
    shared.toast_since.store(now_ms(), Ordering::Release);
    shared.toast.store(cinema::encode_toast(kind, state), Ordering::Release);
    shared.dirty.store(true, Ordering::Release);
}

unsafe fn run_shortcut(hwnd: HWND, st: &mut WndState, shortcut: Shortcut) {
    let shared = st.shared.clone();
    let flip = |flag: &AtomicBool| !flag.fetch_xor(true, Ordering::AcqRel);
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo de interfaz.
    unsafe {
        match shortcut {
            Shortcut::Fullscreen | Shortcut::ExitFullscreen => {
                toggle_fullscreen(hwnd, st);
                show_toast(&shared, ToastKind::Fullscreen, Some(st.fullscreen));
            }
            Shortcut::Record => {
                shared.record_requested.store(true, Ordering::Release);
                show_toast(&shared, ToastKind::Record, Some(!shared.recording.load(Ordering::Acquire)));
            }
            Shortcut::Screenshot => {
                shared.screenshot_requested.store(true, Ordering::Release);
                show_toast(&shared, ToastKind::Screenshot, None);
            }
            Shortcut::Mute => {
                shared.mute_requested.store(true, Ordering::Release);
                show_toast(&shared, ToastKind::Mute, Some(!shared.muted.load(Ordering::Acquire)));
            }
            Shortcut::Blur => show_toast(&shared, ToastKind::Blur, Some(flip(&shared.blur))),
            Shortcut::Cinema => {
                set_borderless(hwnd, st, !st.borderless);
                show_toast(&shared, ToastKind::Cinema, Some(st.borderless));
            }
            Shortcut::Lock => show_toast(&shared, ToastKind::Lock, Some(flip(&shared.locked))),
            Shortcut::Stats => show_toast(&shared, ToastKind::Stats, Some(flip(&shared.stats))),
            Shortcut::TopMost => {
                set_topmost(hwnd, st, !shared.topmost.load(Ordering::Acquire));
                show_toast(&shared, ToastKind::TopMost, Some(shared.topmost.load(Ordering::Acquire)));
            }
        }
    }
}

unsafe fn set_topmost(hwnd: HWND, st: &WndState, on: bool) {
    st.shared.topmost.store(on, Ordering::Release);
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    // SAFETY: llamada Win32 sobre la ventana propia desde su hilo de interfaz.
    unsafe {
        let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

// Rounded corners, a thin accent border and the shadow of the borderless window (Windows 11;
// older versions ignore the attributes).
unsafe fn apply_chrome(hwnd: HWND, bare: bool, fullscreen: bool) {
    const DWMWCP_DEFAULT: i32 = 0;
    const DWMWCP_DONOTROUND: i32 = 1;
    const DWMWCP_ROUND: i32 = 2;
    const COLOR_NONE: u32 = 0xFFFF_FFFE;
    const COLOR_DEFAULT: u32 = 0xFFFF_FFFF;
    let (corners, border, margin) = match (bare, fullscreen) {
        (true, false) => (DWMWCP_ROUND, accent_border(), 1),
        (true, true) => (DWMWCP_DONOTROUND, COLOR_NONE, 0),
        _ => (DWMWCP_DEFAULT, COLOR_DEFAULT, 0),
    };
    // SAFETY: atributos DWM sobre la ventana propia con valores válidos.
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corners as *const i32 as *const c_void, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const u32 as *const c_void, 4);
        let margins = MARGINS { cxLeftWidth: margin, cxRightWidth: margin, cyTopHeight: margin, cyBottomHeight: margin };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
    }
}

// The system accent colour as a COLORREF.
fn accent_border() -> u32 {
    let (mut argb, mut opaque) = (0u32, BOOL(0));
    // SAFETY: punteros locales válidos.
    if unsafe { DwmGetColorizationColor(&mut argb, &mut opaque) }.is_err() {
        return 0x0080_8080;
    }
    let (r, g, b) = ((argb >> 16) & 0xFF, (argb >> 8) & 0xFF, argb & 0xFF);
    r | (g << 8) | (b << 16)
}

// Client rectangle (screen coordinates) of a window rectangle with the given style.
unsafe fn client_of(style: u32, borderless: bool, window: RECT) -> RECT {
    if borderless {
        return window;
    }
    let mut extra = RECT::default();
    // SAFETY: cálculo puro de Win32 sobre un RECT local.
    unsafe {
        let _ = AdjustWindowRectEx(&mut extra, WINDOW_STYLE(style), false, WINDOW_EX_STYLE(0));
    }
    RECT { left: window.left - extra.left, top: window.top - extra.top, right: window.right - extra.right, bottom: window.bottom - extra.bottom }
}

unsafe fn window_of(style: u32, borderless: bool, client: RECT) -> RECT {
    if borderless {
        return client;
    }
    let mut r = client;
    // SAFETY: cálculo puro de Win32 sobre un RECT local.
    unsafe {
        let _ = AdjustWindowRectEx(&mut r, WINDOW_STYLE(style), false, WINDOW_EX_STYLE(0));
    }
    r
}

// Switches between the framed window and the borderless one, keeping the picture where it is.
unsafe fn set_borderless(hwnd: HWND, st: &mut WndState, on: bool) {
    if st.borderless == on {
        return;
    }
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo de interfaz.
    unsafe {
        if st.fullscreen {
            // Only what the window returns to changes.
            let client = client_of(st.style, st.borderless, st.rect);
            st.style = cinema::swap_style(st.style, on);
            st.rect = window_of(st.style, on, client);
        } else {
            let current = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let mut window = RECT::default();
            let _ = GetWindowRect(hwnd, &mut window);
            let client = client_of(current, st.borderless, window);
            let style = cinema::swap_style(current, on);
            // WM_NCCALCSIZE must already see the new mode.
            st.borderless = on;
            SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
            let r = window_of(style, on, client);
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
        st.borderless = on;
        st.shared.borderless.store(on, Ordering::Release);
        st.shared.dirty.store(true, Ordering::Release);
        apply_chrome(hwnd, on, st.fullscreen);
    }
}

fn video_size(st: &WndState) -> Option<(f64, f64)> {
    let w = st.shared.video_width.load(Ordering::Acquire);
    let h = st.shared.video_height.load(Ordering::Acquire);
    (w > 0 && h > 0).then_some((f64::from(w), f64::from(h)))
}

// Window size minus client size, so client rects can be turned into window rects.
unsafe fn frame_extra(hwnd: HWND, borderless: bool) -> (i32, i32) {
    if borderless {
        return (0, 0);
    }
    // SAFETY: consulta del estilo de una ventana propia.
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let mut rc = RECT::default();
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        (rc.right - rc.left, rc.bottom - rc.top)
    }
}

// Sizes the window to the video's aspect ratio (a portrait iPhone gets a portrait window),
// as large as fits in most of the monitor's work area without upscaling. `center` places it in
// the middle of the work area (first appearance) instead of around its current centre.
unsafe fn fit_to_video(hwnd: HWND, st: &WndState, center: bool) {
    let Some((vw, vh)) = video_size(st) else { return };
    if st.fullscreen {
        return;
    }
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo de interfaz.
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork;
        let (ew, eh) = frame_extra(hwnd, st.borderless);
        let max_w = f64::from(work.right - work.left - ew) * FIT_FRACTION;
        let max_h = f64::from(work.bottom - work.top - eh) * FIT_FRACTION;
        let scale = (max_w / vw).min(max_h / vh).min(1.0);
        let w = (vw * scale).round() as i32 + ew;
        let h = (vh * scale).round() as i32 + eh;
        let mut current = RECT::default();
        let _ = GetWindowRect(hwnd, &mut current);
        let (cx, cy) = if center {
            ((work.left + work.right) / 2, (work.top + work.bottom) / 2)
        } else {
            ((current.left + current.right) / 2, (current.top + current.bottom) / 2)
        };
        let x = (cx - w / 2).clamp(work.left, (work.right - w).max(work.left));
        let y = (cy - h / 2).clamp(work.top, (work.bottom - h).max(work.top));
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

unsafe fn toggle_fullscreen(hwnd: HWND, st: &mut WndState) {
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo de interfaz.
    unsafe {
        if !st.fullscreen {
            st.style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let _ = GetWindowRect(hwnd, &mut st.rect);
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return;
            }
            let style = (st.style & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0;
            SetWindowLongPtrW(hwnd, GWL_STYLE, style as isize);
            let r = mi.rcMonitor;
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOOWNERZORDER,
            );
            st.fullscreen = true;
            st.shared.fullscreen.store(true, Ordering::Release);
            st.shared.dirty.store(true, Ordering::Release);
            apply_chrome(hwnd, st.borderless, true);
        } else {
            SetWindowLongPtrW(hwnd, GWL_STYLE, st.style as isize);
            let r = st.rect;
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_FRAMECHANGED | SWP_NOOWNERZORDER | SWP_NOZORDER,
            );
            st.fullscreen = false;
            st.shared.fullscreen.store(false, Ordering::Release);
            st.shared.dirty.store(true, Ordering::Release);
            apply_chrome(hwnd, st.borderless, false);
            if std::mem::take(&mut st.fit_pending) {
                fit_to_video(hwnd, st, false);
            }
        }
    }
}

// The executable's icon resource 1 (RustPlay's logo), if it has one.
fn app_icon(instance: windows::Win32::Foundation::HINSTANCE, size: i32) -> HICON {
    // SAFETY: carga de un recurso del propio módulo; un fallo deja el icono por defecto.
    unsafe {
        LoadImageW(Some(instance), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

fn ui_thread(
    title: String,
    options: (bool, bool, bool),
    shared: Arc<Shared>,
    tx: mpsc::Sender<std::result::Result<isize, String>>,
) {
    // SAFETY: registro de clase, creación de ventana y bucle de mensajes en el hilo propietario.
    unsafe {
        let hinst = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        let class = w!("RustPlayMirrorWindow");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: app_icon(hinst.into(), GetSystemMetrics(SM_CXICON)),
            hIconSm: app_icon(hinst.into(), GetSystemMetrics(SM_CXSMICON)),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let state = Box::into_raw(Box::new(WndState {
            shared: shared.clone(),
            fullscreen: false,
            fit_pending: false,
            tracking: false,
            borderless: false,
            style: 0,
            rect: RECT::default(),
            shown: false,
        }));
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let _ = AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst.into()),
            Some(state as *const c_void),
        );
        let hwnd = match created {
            Ok(h) => h,
            Err(e) => {
                drop(Box::from_raw(state));
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        if options.1 {
            set_topmost(hwnd, &*state, true);
        }
        if options.2 {
            set_borderless(hwnd, &mut *state, true);
        }
        if options.0 {
            let _ = PostMessageW(Some(hwnd), WM_APP_FULLSCREEN, WPARAM(1), LPARAM(0));
        }
        let _ = tx.send(Ok(hwnd.0 as isize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shared.closed.store(true, Ordering::Release);
    }
}

fn letterbox(fw: u32, fh: u32, ow: u32, oh: u32) -> RECT {
    let (dw, dh) = if (ow as u64) * (fh as u64) <= (oh as u64) * (fw as u64) {
        (ow, ((ow as u64) * (fh as u64) / fw.max(1) as u64) as u32)
    } else {
        (((oh as u64) * (fw as u64) / fh.max(1) as u64) as u32, oh)
    };
    let x = (ow - dw) / 2;
    let y = (oh - dh) / 2;
    RECT {
        left: x as i32,
        top: y as i32,
        right: (x + dw) as i32,
        bottom: (y + dh) as i32,
    }
}

impl Presenter {
    pub fn new(gpu: Arc<GpuDevice>, title: &str, options: PresenterOptions) -> Result<Self> {
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            width: AtomicU32::new(1280),
            height: AtomicU32::new(720),
            video_width: AtomicU32::new(0),
            video_height: AtomicU32::new(0),
            fullscreen: AtomicBool::new(false),
            controls: AtomicBool::new(options.controls),
            open_app: AtomicBool::new(options.open_app_button),
            capture: AtomicBool::new(options.capture_buttons),
            recording: AtomicBool::new(false),
            visible_until: AtomicU64::new(0),
            pointer: AtomicU64::new(NO_POINTER),
            pressed: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            open_requested: AtomicBool::new(false),
            record_requested: AtomicBool::new(false),
            screenshot_requested: AtomicBool::new(false),
            mute_requested: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            borderless: AtomicBool::new(options.borderless),
            blur: AtomicBool::new(options.blur_background),
            locked: AtomicBool::new(options.lock_position),
            stats: AtomicBool::new(options.stats),
            topmost: AtomicBool::new(options.always_on_top),
            toast: AtomicU32::new(0),
            toast_since: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let (t, s) = (title.to_owned(), shared.clone());
        let flags = (options.fullscreen, options.always_on_top, options.borderless);
        let ui = std::thread::Builder::new()
            .name("rustplay-presenter-ui".into())
            .spawn(move || ui_thread(t, flags, s, tx))
            .map_err(|e| Error::Media(format!("no se pudo crear el hilo de la ventana: {e}")))?;
        let hwnd = match rx.recv() {
            Ok(Ok(h)) => HWND(h as *mut c_void),
            Ok(Err(e)) => {
                let _ = ui.join();
                return Err(Error::Media(format!("no se pudo crear la ventana: {e}")));
            }
            Err(_) => return Err(Error::Media("el hilo de la ventana terminó".into())),
        };
        let mut this = match Self::init_gfx(gpu, shared, hwnd, options) {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: mensaje a una ventana propia.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
                }
                let _ = ui.join();
                return Err(e);
            }
        };
        this.ui = Some(ui);
        this.show_waiting()?;
        Ok(this)
    }

    fn init_gfx(
        gpu: Arc<GpuDevice>,
        shared: Arc<Shared>,
        hwnd: HWND,
        options: PresenterOptions,
    ) -> Result<Self> {
        let mut rc = RECT::default();
        // SAFETY: ventana válida; el resto son llamadas COM sobre objetos propios.
        unsafe {
            GetClientRect(hwnd, &mut rc)?;
            let (w, h) = (
                (rc.right - rc.left).max(1) as u32,
                (rc.bottom - rc.top).max(1) as u32,
            );
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let adapter = dxgi.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let mut allow = BOOL(0);
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f| {
                    f.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut allow as *mut BOOL as *mut c_void,
                        size_of::<BOOL>() as u32,
                    )
                })
                .is_ok()
                && allow.as_bool();
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            if tearing {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            }
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags,
            };
            let sc1 = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let swap: IDXGISwapChain2 = sc1.cast()?;
            swap.SetMaximumFrameLatency(1)?;
            let waitable = swap.GetFrameLatencyWaitableObject();
            let vdev: ID3D11VideoDevice = gpu.device.cast()?;
            let vctx: ID3D11VideoContext = gpu.context.cast()?;
            Ok(Self {
                gpu,
                shared,
                hwnd,
                ui: None,
                swap,
                flags,
                tearing,
                waitable,
                vdev,
                vctx,
                vp: None,
                vp_in: (0, 0),
                vp_dirty: false,
                cfg: (0, 0, 0, 0, 0),
                cache: Default::default(),
                next_slot: 0,
                buf_size: (w, h),
                target: None,
                options,
                last: None,
                backdrop: None,
                frozen: None,
                paused: None,
                slot_ready: false,
                overlay_alpha: 0.0,
                overlay_clock: Instant::now(),
                recording_since: None,
                drawn: None,
                frames: 0,
                frames_since: Instant::now(),
                fps: 0,
                ambient: None,
                ambient_failed: false,
                ambient_frames: 0,
                d2d_target: None,
                grab: false,
                grabbed: None,
            })
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    fn ensure_target(&mut self) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        // SAFETY: vistas creadas sobre el búfer trasero actual de la cadena de intercambio.
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let mut rtv = None;
            self.gpu
                .device
                .CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut view = None;
            if let Some((en, _)) = &self.vp {
                self.vdev
                    .CreateVideoProcessorOutputView(&back, en, &desc, Some(&mut view))?;
            }
            let rtv =
                rtv.ok_or_else(|| Error::Media("no se pudo crear la vista de destino".into()))?;
            self.target = Some(Target { view, rtv });
        }
        Ok(())
    }

    fn create_vp(&mut self, fw: u32, fh: u32) -> Result<()> {
        let (ow, oh) = self.buf_size;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: fw,
            InputHeight: fh,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: ow,
            OutputHeight: oh,
            Usage: if self.options.high_quality_scaling {
                D3D11_VIDEO_USAGE_OPTIMAL_QUALITY
            } else {
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED
            },
        };
        // SAFETY: descripción válida; el contexto de vídeo es del mismo dispositivo.
        unsafe {
            let en = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = self.vdev.CreateVideoProcessor(&en, 0)?;
            self.vctx
                .VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            self.vctx.VideoProcessorSetStreamOutputRate(
                &vp,
                0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL,
                true,
                None,
            );
            // Entrada YCbCr BT.709 rango limitado; salida RGB rango completo.
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            let output_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            self.vctx
                .VideoProcessorSetStreamColorSpace(&vp, 0, &input_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(&vp, &output_cs);
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            self.vctx
                .VideoProcessorSetOutputBackgroundColor(&vp, false, &black);
            self.vp = Some((en, vp));
        }
        self.vp_in = (fw, fh);
        self.vp_dirty = false;
        self.cfg = (0, 0, 0, 0, 0);
        self.cache = Default::default();
        self.next_slot = 0;
        self.target = None;
        self.last = None;
        self.ambient = None;
        Ok(())
    }

    fn handle_resize(&mut self) -> Result<bool> {
        let w = self.shared.width.load(Ordering::Acquire);
        let h = self.shared.height.load(Ordering::Acquire);
        if w == 0 || h == 0 {
            return Ok(false);
        }
        if (w, h) != self.buf_size {
            self.target = None;
            self.d2d_target = None;
            self.ambient = None;
            // SAFETY: todas las vistas y referencias al búfer trasero se han liberado arriba.
            unsafe {
                self.swap.ResizeBuffers(
                    0,
                    w,
                    h,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(self.flags as i32),
                )?
            };
            self.buf_size = (w, h);
        }
        Ok(true)
    }

    fn present_flags(&self) -> DXGI_PRESENT {
        if self.tearing {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        }
    }

    // Blocks until the swap chain can take another frame, once per Present. Calling it before
    // picking the frame to show lets the caller choose the newest one.
    pub fn wait_ready(&mut self) {
        if !self.slot_ready {
            // SAFETY: espera sobre el objeto de latencia de la cadena de intercambio.
            unsafe { WaitForSingleObjectEx(self.waitable, 50, true) };
            self.slot_ready = true;
        }
    }

    fn swap_present(&mut self) -> Result<()> {
        if std::mem::take(&mut self.grab) {
            // SAFETY: lectura del búfer trasero ya dibujado, antes de presentarlo.
            self.grabbed = unsafe {
                self.swap
                    .GetBuffer::<ID3D11Texture2D>(0)
                    .ok()
                    .and_then(|back| crate::media::screenshot::read_texture(&self.gpu, &back).ok())
            };
        }
        self.slot_ready = false;
        let (interval, flags) = if self.options.vsync { (1, DXGI_PRESENT(0)) } else { (0, self.present_flags()) };
        // SAFETY: cadena de intercambio propia; el búfer trasero ya está dibujado.
        unsafe { self.swap.Present(interval, flags).ok()? };
        Ok(())
    }

    fn scale(&self) -> f32 {
        dpi_scale(self.hwnd)
    }

    // Applies options while the window is open.
    pub fn apply(&mut self, options: PresenterOptions) {
        let old = std::mem::replace(&mut self.options, options);
        let new = &self.options;
        // SAFETY: mensajes a una ventana propia; se atienden en su hilo.
        unsafe {
            if new.fullscreen != old.fullscreen {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_FULLSCREEN, WPARAM(usize::from(new.fullscreen)), LPARAM(0));
            }
            if new.always_on_top != old.always_on_top {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_TOPMOST, WPARAM(usize::from(new.always_on_top)), LPARAM(0));
            }
        }
        if new.borderless != old.borderless {
            // SAFETY: mensaje a una ventana propia; se atiende en su hilo.
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_BORDERLESS, WPARAM(usize::from(new.borderless)), LPARAM(0));
            }
        }
        if new.blur_background != old.blur_background {
            self.shared.blur.store(new.blur_background, Ordering::Release);
            self.ambient_failed = false;
        }
        if new.lock_position != old.lock_position {
            self.shared.locked.store(new.lock_position, Ordering::Release);
        }
        if new.stats != old.stats {
            self.shared.stats.store(new.stats, Ordering::Release);
        }
        if new.high_quality_scaling != old.high_quality_scaling {
            self.vp_dirty = true;
        }
        self.shared.controls.store(new.controls, Ordering::Release);
        self.shared.open_app.store(new.open_app_button, Ordering::Release);
        self.shared.capture.store(new.capture_buttons, Ordering::Release);
        self.shared.dirty.store(true, Ordering::Release);
    }

    // True once after the user pressed the "open the app" button.
    pub fn take_open_request(&self) -> bool {
        self.shared.open_requested.swap(false, Ordering::AcqRel)
    }

    pub fn set_title(&self, title: &str) {
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: ventana propia; su hilo de interfaz atiende WM_SETTEXT.
        unsafe {
            let _ = SetWindowTextW(self.hwnd, PCWSTR(wide.as_ptr()));
        }
    }

    // True once after the user pressed the mute shortcut; the host toggles the audio.
    pub fn take_mute_request(&self) -> bool {
        self.shared.mute_requested.swap(false, Ordering::AcqRel)
    }

    // Tells the window whether this device's audio is muted (for the toast).
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Release);
    }

    // The window as drawn (picture, bars and overlay) at the next presented picture; for tests.
    pub fn grab_next_window(&mut self) {
        self.grab = true;
        self.grabbed = None;
    }

    pub fn take_window_capture(&mut self) -> Option<crate::media::screenshot::Image> {
        self.grabbed.take()
    }

    // True once after the user pressed the record button.
    pub fn take_record_request(&self) -> bool {
        self.shared.record_requested.swap(false, Ordering::AcqRel)
    }

    // True once after the user pressed the screenshot button.
    pub fn take_screenshot_request(&self) -> bool {
        self.shared.screenshot_requested.swap(false, Ordering::AcqRel)
    }

    // Tells the controls whether a recording is running and since when.
    pub fn set_recording(&mut self, since: Option<Instant>) {
        self.recording_since = since;
        self.shared.recording.store(since.is_some(), Ordering::Release);
        self.shared.dirty.store(true, Ordering::Release);
    }

    // The current picture at its own size as BGRA pixels, or None before the first frame.
    // Reads the GPU only when called.
    pub fn capture_frame(&mut self) -> Result<Option<crate::media::screenshot::Image>> {
        let Some(last) = self.last.clone() else { return Ok(None) };
        let (w, h) = last.size;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: w,
            InputHeight: h,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: w,
            OutputHeight: h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let target_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        // SAFETY: procesador, texturas y vistas temporales del mismo dispositivo; se liberan al salir.
        let texture = unsafe {
            let en = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = self.vdev.CreateVideoProcessor(&en, 0)?;
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            let mut texture = None;
            self.gpu.device.CreateTexture2D(&target_desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| Error::Media("no se pudo crear la textura de captura".into()))?;
            let mut out_view = None;
            self.vdev.CreateVideoProcessorOutputView(&texture, &en, &view_desc, Some(&mut out_view))?;
            let out_view = out_view.ok_or_else(|| Error::Media("no se pudo crear la vista de captura".into()))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(last.view.clone())),
                ..Default::default()
            };
            let blt = self.vctx.VideoProcessorBlt(&vp, &out_view, 0, std::slice::from_ref(&stream));
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt?;
            texture
        };
        crate::media::screenshot::read_texture(&self.gpu, &texture).map(Some)
    }

    // How long the caller may wait for video before calling `tick` again.
    pub fn idle_interval(&self) -> Duration {
        if self.overlay_alpha > 0.0 || self.shared.dirty.load(Ordering::Acquire) || self.toast_state().is_some() {
            ANIMATION_INTERVAL
        } else {
            IDLE_INTERVAL
        }
    }

    // The toast being shown and its opacity.
    fn toast_state(&self) -> Option<(u32, f32)> {
        let code = self.shared.toast.load(Ordering::Acquire);
        if code == 0 {
            return None;
        }
        let elapsed = now_ms().saturating_sub(self.shared.toast_since.load(Ordering::Acquire));
        cinema::toast_alpha(elapsed).map(|alpha| (code, alpha))
    }

    // Advances the fade of the controls and returns what the overlay should look like now.
    fn overlay_key(&mut self) -> OverlayKey {
        let now = Instant::now();
        let dt = now.duration_since(self.overlay_clock).as_secs_f32().min(0.1);
        self.overlay_clock = now;
        let scale = self.scale();
        let step = dt / overlay::FADE_SECONDS;
        self.overlay_alpha = if self.shared.overlay_wanted(scale) {
            (self.overlay_alpha + step).min(1.0)
        } else {
            (self.overlay_alpha - step).max(0.0)
        };
        OverlayKey {
            alpha: self.overlay_alpha,
            hovered: self.shared.hovered(scale),
            pressed: self.shared.pressed.load(Ordering::Acquire) != 0,
            fullscreen: self.shared.fullscreen.load(Ordering::Acquire),
            fps: (self.shared.stats.load(Ordering::Acquire) && self.last.is_some()).then_some(self.fps),
            recording: self
                .recording_since
                .filter(|_| self.overlay_alpha > 0.0)
                .map(|since| since.elapsed().as_secs()),
            toast: self.toast_state(),
        }
    }

    fn count_frame(&mut self) {
        self.frames += 1;
        self.update_fps();
    }

    fn update_fps(&mut self) {
        let elapsed = self.frames_since.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.fps = (self.frames as f32 / elapsed.as_secs_f32()).round() as u32;
            self.frames = 0;
            self.frames_since = Instant::now();
        }
    }

    fn overlay_fonts(&mut self, scale: f32) -> Result<overlay::Fonts> {
        let backdrop = self.backdrop.as_mut().ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        if let Some(fonts) = backdrop.fonts.as_ref().filter(|f| f.scale == scale) {
            return Ok(fonts.clone());
        }
        let mut collection = None;
        // SAFETY: consulta de la colección de fuentes del sistema.
        unsafe {
            let _ = backdrop.dwrite.GetSystemFontCollection(&mut collection, false);
        }
        let style = Style { dwrite: &backdrop.dwrite, coll: collection.as_ref() };
        let fonts = overlay::Fonts {
            scale,
            icon: style.format(&["Segoe Fluent Icons", "Segoe MDL2 Assets"], DWRITE_FONT_WEIGHT_NORMAL, 16.0 * scale, true)?,
            label: style.format(&["Segoe UI Variable Text", "Segoe UI"], DWRITE_FONT_WEIGHT_NORMAL, 12.0 * scale, true)?,
        };
        backdrop.fonts = Some(fonts.clone());
        Ok(fonts)
    }

    // Draws statistics and controls into the target bound to `dc` (between BeginDraw and EndDraw).
    fn paint_overlay(&mut self, dc: &ID2D1DeviceContext, dwrite: &IDWriteFactory, key: OverlayKey) -> Result<()> {
        if key.alpha <= 0.0 && key.fps.is_none() && key.toast.is_none() {
            return Ok(());
        }
        let fonts = self.overlay_fonts(self.scale())?;
        let stats = match (key.fps, &self.last) {
            (Some(fps), Some(last)) => Some(
                self.options
                    .stats_text
                    .replace("{width}", &last.size.0.to_string())
                    .replace("{height}", &last.size.1.to_string())
                    .replace("{fps}", &fps.to_string()),
            ),
            _ => None,
        };
        let texts = &self.options.overlay;
        let toast = key.toast.and_then(|(code, alpha)| {
            let (kind, state) = cinema::decode_toast(code)?;
            let name = match kind {
                ToastKind::Fullscreen => &texts.toast_fullscreen,
                ToastKind::Record => &texts.toast_recording,
                ToastKind::Screenshot => &texts.toast_screenshot,
                ToastKind::Mute => &texts.toast_mute,
                ToastKind::Blur => &texts.toast_blur,
                ToastKind::Cinema => &texts.toast_cinema,
                ToastKind::Lock => &texts.toast_lock,
                ToastKind::Stats => &texts.toast_stats,
                ToastKind::TopMost => &texts.toast_topmost,
            };
            Some((cinema::toast_text(name, state, &texts.toast_on, &texts.toast_off), alpha))
        });
        let label = |action: Action, fullscreen: bool, recording: bool| -> &str {
            match action {
                Action::Fullscreen if fullscreen => &texts.exit_fullscreen,
                Action::Fullscreen => &texts.fullscreen,
                Action::OpenApp => &texts.open_app,
                Action::Record if recording => &texts.stop_recording,
                Action::Record => &texts.record,
                Action::Screenshot => &texts.screenshot,
                Action::Stop => &texts.stop,
            }
        };
        let view = overlay::View {
            alpha: key.alpha,
            hovered: key.hovered,
            pressed: key.pressed,
            fullscreen: key.fullscreen,
            recording: key.recording,
            actions: self.shared.actions(),
            label: &label,
            stats: stats.as_deref(),
            toast: toast.as_ref().map(|(text, alpha)| (text.as_str(), *alpha)),
        };
        // SAFETY: el llamador tiene el contexto entre BeginDraw y EndDraw.
        unsafe { overlay::draw(dc, dwrite, &fonts, self.buf_size, &view) }
    }

    // Draws the overlay on the current back buffer (already holding the video).
    fn overlay_pass(&mut self, key: OverlayKey) -> Result<()> {
        self.drawn = Some(key);
        let bars = self.ambient_bars();
        if key.alpha <= 0.0 && key.fps.is_none() && key.toast.is_none() && bars.iter().all(Option::is_none) {
            return Ok(());
        }
        let (dc, dwrite) = self.ensure_backdrop()?;
        let target = match self.d2d_target.clone() {
            Some(target) => target,
            None => {
                // SAFETY: el mapa de bits se suelta (`d2d_target = None`) antes de redimensionar la cadena.
                let target = unsafe {
                    let back: IDXGISurface = self.swap.GetBuffer(0)?;
                    let props = Self::bitmap_props(D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
                    dc.CreateBitmapFromDxgiSurface(&back, Some(&props))?
                };
                self.d2d_target = Some(target.clone());
                target
            }
        };
        // SAFETY: el destino se desvincula del contexto (SetTarget(None)) antes de volver.
        unsafe {
            dc.SetTarget(&target);
            dc.BeginDraw();
            let drawn = self.paint_ambient(&dc, bars).and_then(|()| self.paint_overlay(&dc, &dwrite, key));
            let ended = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            drawn?;
            ended?;
        }
        Ok(())
    }

    pub fn show_waiting(&mut self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        self.wait_ready();
        if !self.handle_resize()? {
            return Ok(());
        }
        self.clear_black()?;
        let key = self.overlay_key();
        self.overlay_pass(key)?;
        self.swap_present()
    }

    fn clear_black(&mut self) -> Result<()> {
        self.ensure_target()?;
        if let Some(t) = &self.target {
            // SAFETY: RTV válida sobre el búfer trasero actual.
            unsafe { self.gpu.context.ClearRenderTargetView(&t.rtv, &[0.0, 0.0, 0.0, 1.0]) };
        }
        Ok(())
    }

    // Redraws when the overlay, the statistics or the window size changed; called while no video
    // arrives (a still iPhone screen sends no frames).
    pub fn tick(&mut self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        if self.shared.stats.load(Ordering::Acquire) && self.frames_since.elapsed() >= Duration::from_secs(2) {
            self.update_fps();
        }
        let size = (self.shared.width.load(Ordering::Acquire), self.shared.height.load(Ordering::Acquire));
        let resized = size != self.buf_size && size.0 > 0 && size.1 > 0;
        let dirty = self.shared.dirty.swap(false, Ordering::AcqRel);
        let key = self.overlay_key();
        if !resized && !dirty && self.drawn == Some(key) {
            return Ok(());
        }
        if self.paused.is_some() {
            return self.render_paused();
        }
        self.wait_ready();
        if !self.handle_resize()? {
            return Ok(());
        }
        match self.last.clone() {
            Some(last) => {
                self.ensure_target()?;
                self.update_ambient(&last.view, last.size, false);
                self.blit(&last.view, last.size)?;
            }
            None => self.clear_black()?,
        }
        self.overlay_pass(key)?;
        self.swap_present()
    }

    fn input_view(&mut self, frame: &DecodedFrame) -> Result<ID3D11VideoProcessorInputView> {
        let key = Interface::as_raw(&frame.texture);
        if let Some(view) = self
            .cache
            .iter()
            .flatten()
            .find(|c| Interface::as_raw(&c.texture) == key && c.slice == frame.subresource)
            .map(|c| c.view.clone())
        {
            return Ok(view);
        }
        let en = self
            .vp
            .as_ref()
            .map(|(e, _)| e.clone())
            .ok_or_else(|| Error::Media("sin procesador de vídeo".into()))?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: frame.subresource },
            },
        };
        let mut view = None;
        // SAFETY: textura del decodificador y enumerador del mismo dispositivo.
        unsafe {
            self.vdev
                .CreateVideoProcessorInputView(&frame.texture, &en, &desc, Some(&mut view))?;
        }
        let view = view.ok_or_else(|| Error::Media("no se pudo crear la vista de entrada".into()))?;
        self.cache[self.next_slot] = Some(CachedView {
            texture: frame.texture.clone(),
            slice: frame.subresource,
            view: view.clone(),
        });
        self.next_slot = (self.next_slot + 1) % INPUT_VIEW_CACHE;
        Ok(view)
    }

    // Converts and scales a decoded frame into the back buffer on the GPU.
    fn blit(&mut self, view: &ID3D11VideoProcessorInputView, (fw, fh): (u32, u32)) -> Result<()> {
        let (ow, oh) = self.buf_size;
        let cfg = (fw, fh, ow, oh, self.options.scaling as u32);
        let (
            Some((_, vp)),
            Some(Target {
                view: Some(out_view),
                ..
            }),
        ) = (&self.vp, &self.target)
        else {
            return Ok(());
        };
        // SAFETY: vistas y procesador creados sobre el mismo dispositivo; el flujo se libera tras el Blt.
        unsafe {
            if cfg != self.cfg {
                let (src, dst) = placement((fw, fh), (ow, oh), self.options.scaling);
                let full = RECT { left: 0, top: 0, right: ow as i32, bottom: oh as i32 };
                self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
                self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
                self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&full));
                self.cfg = cfg;
            }
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(view.clone())),
                ..Default::default()
            };
            let blt = self.vctx.VideoProcessorBlt(vp, out_view, 0, std::slice::from_ref(&stream));
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt?;
        }
        Ok(())
    }

    // Se llama desde el hilo de vídeo; la conversión NV12 a BGRA ocurre en la GPU.
    pub fn present(&mut self, frame: &DecodedFrame) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        if self.paused.take().is_some() {
            self.frozen = None;
        }
        self.wait_ready();
        if !self.handle_resize()? {
            return Ok(());
        }
        let size = (frame.width, frame.height);
        if self.vp.is_none() || self.vp_in != size || self.vp_dirty {
            if self.vp_in != size {
                self.shared.video_width.store(frame.width, Ordering::Release);
                self.shared.video_height.store(frame.height, Ordering::Release);
                // SAFETY: mensaje a una ventana propia; el ajuste se hace en su hilo.
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_APP_FIT, WPARAM(0), LPARAM(0));
                }
            }
            self.create_vp(frame.width, frame.height)?;
        }
        self.ensure_target()?;
        let view = self.input_view(frame)?;
        self.last = Some(LastFrame { view: view.clone(), size });
        self.ambient_frames = self.ambient_frames.wrapping_add(1);
        self.update_ambient(&view, size, cinema::ambient_refresh_due(self.ambient_frames));
        self.blit(&view, size)?;
        self.count_frame();
        let key = self.overlay_key();
        self.overlay_pass(key)?;
        self.swap_present()
    }
}

// Source and destination rectangles of the video for the chosen scaling mode.
fn placement((fw, fh): (u32, u32), (ow, oh): (u32, u32), scaling: Scaling) -> (RECT, RECT) {
    let whole = RECT { left: 0, top: 0, right: fw as i32, bottom: fh as i32 };
    let full = RECT { left: 0, top: 0, right: ow as i32, bottom: oh as i32 };
    match scaling {
        Scaling::Fit => (whole, letterbox(fw, fh, ow, oh)),
        Scaling::Stretch => (whole, full),
        Scaling::Fill => (crop(fw, fh, ow, oh), full),
    }
}

// The centred part of the video with the output's aspect ratio.
fn crop(fw: u32, fh: u32, ow: u32, oh: u32) -> RECT {
    let (fw64, fh64, ow64, oh64) = (u64::from(fw), u64::from(fh), u64::from(ow.max(1)), u64::from(oh.max(1)));
    if fw64 * oh64 > ow64 * fh64 {
        let cw = (fh64 * ow64 / oh64) as u32;
        let x = (fw - cw) / 2;
        RECT { left: x as i32, top: 0, right: (x + cw) as i32, bottom: fh as i32 }
    } else {
        let ch = (fw64 * oh64 / ow64) as u32;
        let y = (fh - ch) / 2;
        RECT { left: 0, top: y as i32, right: fw as i32, bottom: (y + ch) as i32 }
    }
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn pick_font(coll: Option<&IDWriteFontCollection>, names: &[&str]) -> windows::core::HSTRING {
    if let Some(coll) = coll {
        for name in names {
            let wide = windows::core::HSTRING::from(*name);
            let (mut index, mut exists) = (0u32, BOOL(0));
            // SAFETY: consulta de solo lectura sobre la colección de fuentes del sistema.
            let found = unsafe { coll.FindFamilyName(&wide, &mut index, &mut exists) };
            if found.is_ok() && exists.as_bool() {
                return wide;
            }
        }
    }
    windows::core::HSTRING::from(*names.last().unwrap_or(&"Segoe UI"))
}

struct Style<'a> {
    dwrite: &'a IDWriteFactory,
    coll: Option<&'a IDWriteFontCollection>,
}

impl Style<'_> {
    fn format(
        &self,
        families: &[&str],
        weight: DWRITE_FONT_WEIGHT,
        size: f32,
        vertical_center: bool,
    ) -> Result<IDWriteTextFormat> {
        let family = pick_font(self.coll, families);
        // SAFETY: creación de un formato de texto con parámetros válidos.
        unsafe {
            let fmt = self.dwrite.CreateTextFormat(
                &family,
                None::<&IDWriteFontCollection>,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("es-ES"),
            )?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt.SetParagraphAlignment(if vertical_center {
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER
            } else {
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR
            })?;
            Ok(fmt)
        }
    }
}

// Draws the lock-screen backdrop; `scale` is the window DPI / 96.
unsafe fn draw_backdrop(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    source: Option<(&ID2D1Bitmap1, (u32, u32))>,
    (w, h): (u32, u32),
    scale: f32,
    (title_text, message_text): (&str, &str),
) -> Result<()> {
    let (wf, hf) = (w as f32, h as f32);
    let full = D2D_RECT_F { left: 0.0, top: 0.0, right: wf, bottom: hf };
    // SAFETY: todas las llamadas D2D/DWrite usan objetos válidos del mismo dispositivo.
    unsafe {
        let black = color(0.0, 0.0, 0.0, 1.0);
        match source {
            Some((bitmap, (vw, vh))) => {
                let s = (wf / vw as f32).max(hf / vh as f32);
                let blur = dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
                blur.SetInput(0, bitmap, true);
                blur.SetValue(
                    D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &(40.0 * scale / s).to_ne_bytes(),
                )?;
                blur.SetValue(
                    D2D1_GAUSSIANBLUR_PROP_BORDER_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &(D2D1_BORDER_MODE_HARD.0 as u32).to_ne_bytes(),
                )?;
                let out = blur.GetOutput()?;
                dc.Clear(Some(&black));
                dc.SetTransform(&Matrix3x2 {
                    M11: s,
                    M12: 0.0,
                    M21: 0.0,
                    M22: s,
                    M31: (wf - vw as f32 * s) / 2.0,
                    M32: (hf - vh as f32 * s) / 2.0,
                });
                dc.DrawImage(
                    &out,
                    None,
                    None,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    D2D1_COMPOSITE_MODE_SOURCE_OVER,
                );
                dc.SetTransform(&Matrix3x2::identity());
            }
            None => {
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: color(0.16, 0.19, 0.30, 1.0) },
                    D2D1_GRADIENT_STOP { position: 1.0, color: color(0.03, 0.03, 0.07, 1.0) },
                ];
                let coll =
                    dc.CreateGradientStopCollection(
                        &stops,
                        D2D1_COLOR_SPACE_SRGB,
                        D2D1_COLOR_SPACE_SRGB,
                        D2D1_BUFFER_PRECISION_8BPC_UNORM,
                        D2D1_EXTEND_MODE_CLAMP,
                        D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT,
                    )?;
                let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: Vector2 { X: 0.0, Y: 0.0 },
                    endPoint: Vector2 { X: wf, Y: hf },
                };
                let brush = dc.CreateLinearGradientBrush(&props, None, &coll)?;
                dc.FillRectangle(&full, &brush);
            }
        }
        let shade = dc.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 0.55), None)?;
        dc.FillRectangle(&full, &shade);

        let circle = 96.0 * scale;
        let gap = 24.0 * scale;
        let title_h = 40.0 * scale;
        let small_gap = 6.0 * scale;
        let sub_h = 44.0 * scale;
        let total = circle + gap + title_h + small_gap + sub_h;
        let top = ((hf - total) / 2.0).max(0.0);
        let (cx, cy) = (wf / 2.0, top + circle / 2.0);
        let white = |a: f32| color(1.0, 1.0, 1.0, a);

        let fill = dc.CreateSolidColorBrush(&white(0.12), None)?;
        dc.FillEllipse(
            &D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: circle / 2.0,
                radiusY: circle / 2.0,
            },
            &fill,
        );

        let mut collection = None;
        let _ = dwrite.GetSystemFontCollection(&mut collection, false);
        let style = Style { dwrite, coll: collection.as_ref() };
        dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let text_w = (wf - 48.0 * scale).clamp(1.0, 560.0 * scale);
        let left = (wf - text_w) / 2.0;
        let none = D2D1_DRAW_TEXT_OPTIONS_NONE;
        let natural = DWRITE_MEASURING_MODE_NATURAL;

        let icon_fmt = style.format(
            &["Segoe Fluent Icons", "Segoe MDL2 Assets"],
            DWRITE_FONT_WEIGHT_NORMAL,
            40.0 * scale,
            true,
        )?;
        let solid = dc.CreateSolidColorBrush(&white(1.0), None)?;
        let icon_rect = D2D_RECT_F {
            left: cx - circle / 2.0,
            top,
            right: cx + circle / 2.0,
            bottom: top + circle,
        };
        let icon: Vec<u16> = "\u{E72E}".encode_utf16().collect();
        dc.DrawText(&icon, &icon_fmt, &icon_rect, &solid, none, natural);

        let title_fmt = style.format(
            &["Segoe UI Variable Display", "Segoe UI"],
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            28.0 * scale,
            false,
        )?;
        let title_top = top + circle + gap;
        let title_rect = D2D_RECT_F {
            left,
            top: title_top,
            right: left + text_w,
            bottom: title_top + title_h,
        };
        let title: Vec<u16> = title_text.encode_utf16().collect();
        dc.DrawText(&title, &title_fmt, &title_rect, &solid, none, natural);

        let sub_fmt = style.format(
            &["Segoe UI Variable Text", "Segoe UI"],
            DWRITE_FONT_WEIGHT_NORMAL,
            15.0 * scale,
            false,
        )?;
        let sub_brush = dc.CreateSolidColorBrush(&white(0.75), None)?;
        let sub_top = title_top + title_h + small_gap;
        let sub_rect = D2D_RECT_F {
            left,
            top: sub_top,
            right: left + text_w,
            bottom: sub_top + sub_h,
        };
        let sub: Vec<u16> = message_text.encode_utf16().collect();
        dc.DrawText(&sub, &sub_fmt, &sub_rect, &sub_brush, none, natural);
    }
    Ok(())
}

impl Presenter {
    // Whether the picture leaves bars around it that should show the blurred background.
    fn ambient_wanted(&self, (fw, fh): (u32, u32)) -> bool {
        let (ow, oh) = self.buf_size;
        self.shared.blur.load(Ordering::Acquire)
            && !self.ambient_failed
            && self.options.scaling == Scaling::Fit
            && cinema::bars(letterbox(fw, fh, ow, oh), self.buf_size).iter().any(Option::is_some)
    }

    // The strips to paint with the blurred layer right now.
    fn ambient_bars(&self) -> [Option<RECT>; 2] {
        let Some(last) = &self.last else { return [None, None] };
        let ready = self.ambient.as_ref().is_some_and(|a| a.filled);
        if ready && self.ambient_wanted(last.size) {
            let (ow, oh) = self.buf_size;
            cinema::bars(letterbox(last.size.0, last.size.1, ow, oh), self.buf_size)
        } else {
            [None, None]
        }
    }

    // Keeps the low-resolution layer up to date; any failure falls back to black bars.
    fn update_ambient(&mut self, view: &ID3D11VideoProcessorInputView, size: (u32, u32), due: bool) {
        if !self.ambient_wanted(size) {
            return;
        }
        if let Err(e) = self.refresh_ambient(view, size, due) {
            tracing::warn!("no se pudo preparar el fondo difuminado; se usan barras negras: {e}");
            self.ambient = None;
            self.ambient_failed = true;
        }
    }

    fn create_ambient(&mut self, size: (u32, u32)) -> Result<Ambient> {
        let (dc, _) = self.ensure_backdrop()?;
        let Some((en, _)) = &self.vp else {
            return Err(Error::Media("sin procesador de vídeo".into()));
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        // SAFETY: textura, vistas y efecto D2D creados sobre el mismo dispositivo que el procesador.
        unsafe {
            let mut texture = None;
            self.gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.ok_or_else(|| Error::Media("no se pudo crear la textura del fondo".into()))?;
            let mut out_view = None;
            self.vdev.CreateVideoProcessorOutputView(&texture, en, &view_desc, Some(&mut out_view))?;
            let out_view = out_view.ok_or_else(|| Error::Media("no se pudo crear la vista del fondo".into()))?;
            let surface: IDXGISurface = texture.cast()?;
            let bitmap = dc.CreateBitmapFromDxgiSurface(&surface, Some(&Self::bitmap_props(D2D1_BITMAP_OPTIONS_NONE)))?;
            let blur = dc.CreateEffect(&CLSID_D2D1GaussianBlur)?;
            blur.SetInput(0, &bitmap, true);
            blur.SetValue(
                D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
                D2D1_PROPERTY_TYPE_FLOAT,
                &cinema::AMBIENT_SIGMA.to_ne_bytes(),
            )?;
            blur.SetValue(
                D2D1_GAUSSIANBLUR_PROP_BORDER_MODE.0 as u32,
                D2D1_PROPERTY_TYPE_ENUM,
                &(D2D1_BORDER_MODE_HARD.0 as u32).to_ne_bytes(),
            )?;
            let image = blur.GetOutput()?;
            let dim = dc.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, cinema::AMBIENT_DIM), None)?;
            Ok(Ambient { size, _texture: texture, out_view, _bitmap: bitmap, image, dim, filled: false })
        }
    }

    fn refresh_ambient(&mut self, view: &ID3D11VideoProcessorInputView, (fw, fh): (u32, u32), due: bool) -> Result<()> {
        let (ow, oh) = self.buf_size;
        let want = cinema::ambient_size(ow, oh);
        if self.ambient.as_ref().is_none_or(|a| a.size != want) {
            self.ambient = Some(self.create_ambient(want)?);
        }
        let (Some((_, vp)), Some(ambient)) = (&self.vp, &mut self.ambient) else {
            return Ok(());
        };
        if ambient.filled && !due {
            return Ok(());
        }
        // The same picture the window shows in "Fill" mode, scaled down into the whole layer.
        let (src, dst) = cinema::ambient_placement(crop(fw, fh, ow, oh), want);
        // SAFETY: vistas y procesador del mismo dispositivo; el flujo se libera tras el Blt.
        unsafe {
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
            self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(view.clone())),
                ..Default::default()
            };
            let blt = self.vctx.VideoProcessorBlt(vp, &ambient.out_view, 0, std::slice::from_ref(&stream));
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt?;
        }
        ambient.filled = true;
        // The next blit into the back buffer sets its own rectangles again.
        self.cfg = (0, 0, 0, 0, 0);
        Ok(())
    }

    // Paints the blurred layer, darkened, over the black bars (inside BeginDraw / EndDraw).
    fn paint_ambient(&self, dc: &ID2D1DeviceContext, bars: [Option<RECT>; 2]) -> Result<()> {
        let Some(ambient) = self.ambient.as_ref().filter(|_| bars.iter().any(Option::is_some)) else {
            return Ok(());
        };
        let (sx, sy) = cinema::ambient_scale(ambient.size, self.buf_size);
        let stretch = Matrix3x2 { M11: sx, M12: 0.0, M21: 0.0, M22: sy, M31: 0.0, M32: 0.0 };
        // SAFETY: el llamador tiene el contexto entre BeginDraw y EndDraw.
        unsafe {
            for bar in bars.into_iter().flatten() {
                let clip = D2D_RECT_F { left: bar.left as f32, top: bar.top as f32, right: bar.right as f32, bottom: bar.bottom as f32 };
                dc.SetTransform(&Matrix3x2::identity());
                dc.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
                dc.SetTransform(&stretch);
                dc.DrawImage(&ambient.image, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
                dc.SetTransform(&Matrix3x2::identity());
                dc.FillRectangle(&clip, &ambient.dim);
                dc.PopAxisAlignedClip();
            }
        }
        Ok(())
    }
}

impl Presenter {
    fn ensure_backdrop(&mut self) -> Result<(ID2D1DeviceContext, IDWriteFactory)> {
        if self.backdrop.is_none() {
            // SAFETY: fábrica D2D multihilo sobre el dispositivo D3D11 compartido con el decodificador.
            unsafe {
                let factory: ID2D1Factory1 =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)?;
                let dxgi: IDXGIDevice = self.gpu.device.cast()?;
                let device = factory.CreateDevice(&dxgi)?;
                let dc = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                dc.SetDpi(96.0, 96.0);
                let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
                self.backdrop = Some(Backdrop { dc, dwrite, fonts: None });
            }
        }
        let b = self
            .backdrop
            .as_ref()
            .ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        Ok((b.dc.clone(), b.dwrite.clone()))
    }

    fn bitmap_props(options: D2D1_BITMAP_OPTIONS) -> D2D1_BITMAP_PROPERTIES1 {
        D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: options,
            colorContext: ManuallyDrop::new(None),
        }
    }

    // Copies the last presented frame (converted to BGRA on the GPU) into an offscreen bitmap.
    fn capture_frozen(&mut self, dc: &ID2D1DeviceContext) -> Result<()> {
        let (Some(last), Some((en, vp))) = (&self.last, &self.vp) else {
            return Ok(());
        };
        let (vw, vh) = last.size;
        // SAFETY: textura, vistas y procesador de vídeo del mismo dispositivo; el flujo se libera
        // tras el Blt.
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: vw,
                Height: vh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut tex = None;
            self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(|| Error::Media("no se pudo crear la textura".into()))?;
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut ov = None;
            self.vdev
                .CreateVideoProcessorOutputView(&tex, en, &ov_desc, Some(&mut ov))?;
            let ov =
                ov.ok_or_else(|| Error::Media("no se pudo crear la vista de salida".into()))?;
            let rect = RECT { left: 0, top: 0, right: vw as i32, bottom: vh as i32 };
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&rect));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&rect));
            self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&rect));
            self.cfg = (0, 0, 0, 0, 0);
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(last.view.clone())),
                ..Default::default()
            };
            let blt = self
                .vctx
                .VideoProcessorBlt(vp, &ov, 0, std::slice::from_ref(&stream));
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt?;
            let surface: IDXGISurface = tex.cast()?;
            let props = Self::bitmap_props(D2D1_BITMAP_OPTIONS_NONE);
            let bitmap = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
            self.frozen = Some(Frozen { _texture: tex, bitmap, size: (vw, vh) });
        }
        Ok(())
    }

    fn render_paused(&mut self) -> Result<()> {
        self.wait_ready();
        if !self.handle_resize()? {
            return Ok(());
        }
        let (dc, dwrite) = self.ensure_backdrop()?;
        let size = self.buf_size;
        let scale = self.scale();
        let key = self.overlay_key();
        self.drawn = Some(key);
        // SAFETY: el mapa de bits del búfer trasero se libera (SetTarget(None)) antes de volver.
        unsafe {
            let back: IDXGISurface = self.swap.GetBuffer(0)?;
            let props =
                Self::bitmap_props(D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW);
            let target = dc.CreateBitmapFromDxgiSurface(&back, Some(&props))?;
            dc.SetTarget(&target);
            dc.BeginDraw();
            let source = self.frozen.as_ref().map(|f| (&f.bitmap, f.size));
            let texts = (self.options.lock_title.as_str(), self.options.lock_message.as_str());
            let drawn = draw_backdrop(&dc, &dwrite, source, size, scale, texts)
                .and_then(|()| self.paint_overlay(&dc, &dwrite, key));
            let ended = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            drop(target);
            drawn?;
            ended?;
        }
        self.swap_present()?;
        self.paused = Some(size);
        Ok(())
    }

    // Shows the lock-screen backdrop: blurred last frame (or a gradient) plus lock icon and text.
    pub fn show_paused(&mut self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        self.frozen = None;
        let (dc, _) = self.ensure_backdrop()?;
        if let Err(e) = self.capture_frozen(&dc) {
            tracing::warn!("no se pudo capturar el último fotograma: {e}");
            self.frozen = None;
        }
        self.render_paused()
    }
}

#[cfg(test)]
impl Presenter {
    // Resizes the window to an outer size, as a user dragging an edge would.
    pub(super) fn test_resize(&self, w: i32, h: i32) {
        // SAFETY: ventana propia.
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, 40, 40, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    pub(super) fn test_key(&self, vk: usize) {
        // SAFETY: mensaje a una ventana propia; el hilo de interfaz lo atiende.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_KEYDOWN, WPARAM(vk), LPARAM(0));
        }
    }

    pub(super) fn test_style(&self) -> u32 {
        // SAFETY: consulta de una ventana propia.
        unsafe { GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32 }
    }

    pub(super) fn test_client_size(&self) -> (u32, u32) {
        let mut rc = RECT::default();
        // SAFETY: consulta de una ventana propia.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        ((rc.right - rc.left) as u32, (rc.bottom - rc.top) as u32)
    }

    pub(super) fn test_state(&self) -> (bool, bool, bool, bool) {
        let s = &self.shared;
        (
            s.borderless.load(Ordering::Acquire),
            s.blur.load(Ordering::Acquire),
            s.locked.load(Ordering::Acquire),
            s.topmost.load(Ordering::Acquire),
        )
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // SAFETY: mensaje a la ventana propia; el hilo de interfaz la destruye y termina.
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(ui) = self.ui.take() {
            let _ = ui.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    #[test]
    fn scaling_modes_place_the_video() {
        // A portrait 1000×2000 video in a 1000×1000 window.
        let (src, dst) = placement((1000, 2000), (1000, 1000), Scaling::Fit);
        assert_eq!(rect(src), (0, 0, 1000, 2000));
        assert_eq!(rect(dst), (250, 0, 750, 1000));
        let (src, dst) = placement((1000, 2000), (1000, 1000), Scaling::Fill);
        assert_eq!(rect(src), (0, 500, 1000, 1500));
        assert_eq!(rect(dst), (0, 0, 1000, 1000));
        let (src, dst) = placement((1000, 2000), (1000, 1000), Scaling::Stretch);
        assert_eq!(rect(src), (0, 0, 1000, 2000));
        assert_eq!(rect(dst), (0, 0, 1000, 1000));
        // A landscape video in a portrait window crops the sides.
        assert_eq!(rect(crop(1920, 1080, 1080, 1920)), (656, 0, 1263, 1080));
    }
}
