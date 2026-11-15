// Video player for AirPlay "casting" (the client sends a URL instead of mirroring its screen).
// The engine (engine.rs: RustPlay's own HLS player, or the Windows MediaPlayer for plain files)
// plays the URL or playlist (audio included); each frame is copied by the GPU into a swap chain of our own window, where Direct2D draws
// the controls (volume, play/pause, subtitles, quality, speed, full screen, stop, seek bar and the
// flyout menus) in the same style as the mirror window.
// Everything lives on one dedicated thread: window, message loop, engine and rendering.
mod engine;
mod native;
mod layout;

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, Interface, PCWSTR, w};

use self::layout::{Button, FADE_SECONDS, Features, HIDE_AFTER_MS, Menu};
use self::native::{Quality, SPEEDS, step_speed};
use super::gpu::GpuDevice;
use crate::{Error, Result};

const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;
const VK_SPACE: usize = 0x20;
const VK_LEFT: usize = 0x25;
const VK_UP: usize = 0x26;
const VK_RIGHT: usize = 0x27;
const VK_DOWN: usize = 0x28;
const VK_F: usize = 0x46;
const VK_M: usize = 0x4D;
const VK_F11: usize = 0x7A;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HIDE: usize = 1;
const NO_POINTER: u64 = u64::MAX;
const FIT_FRACTION: f64 = 0.9;
const LOOP_INTERVAL: Duration = Duration::from_millis(8);
const STATUS_INTERVAL: Duration = Duration::from_millis(100);
const ACTION_PLAY_PAUSE: u32 = 1;
const ACTION_STOP: u32 = 2;
const ACTION_SEEK: u32 = 4;
const ACTION_MUTE: u32 = 8;
const ACTION_VOLUME_SET: u32 = 16;
const ACTION_VOLUME_STEP: u32 = 32;
const ACTION_SEEK_RELATIVE: u32 = 64;
const ACTION_SPEED_STEP: u32 = 128;
const ACTION_PICK: u32 = 256;
// Seconds the arrow keys seek by.
const SEEK_STEP_SECONDS: i32 = 5;
// The menu contents are rebuilt this often while a menu is open (the automatic quality changes).
const MENU_REFRESH: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StartAt {
    Seconds(f64),
    // Fraction of the duration, known only once the media is open.
    Fraction(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerCommand {
    Play,
    Pause,
    Seek(f64),
    // Linear volume, 0.0 (silent) to 1.0.
    Volume(f32),
    Stop,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PlaybackStatus {
    // The media's metadata is loaded (duration and size are known).
    pub opened: bool,
    // The window was closed or stopped.
    pub closed: bool,
    pub ended: bool,
    pub failed: bool,
    pub playing: bool,
    pub buffering: bool,
    pub duration: f64,
    pub position: f64,
    // Playback speed (1.0 normal); 0.0 before the media is open.
    pub rate: f64,
}

// Already translated tooltips of the player's controls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerTexts {
    pub play: String,
    pub pause: String,
    pub fullscreen: String,
    pub exit_fullscreen: String,
    pub stop: String,
    pub volume: String,
    pub mute: String,
    pub unmute: String,
    pub speed: String,
    pub quality: String,
    pub quality_auto: String,
    pub subtitles: String,
    pub subtitles_off: String,
    pub live: String,
}

#[derive(Debug, Clone, Default)]
pub struct PlayerOptions {
    pub title: String,
    pub fullscreen: bool,
    pub always_on_top: bool,
    pub controls: bool,
    pub texts: PlayerTexts,
}

struct Shared {
    status: Mutex<PlaybackStatus>,
    closed: AtomicBool,
}

pub struct VideoPlayer {
    shared: Arc<Shared>,
    commands: mpsc::Sender<PlayerCommand>,
    thread: Option<JoinHandle<()>>,
}

impl VideoPlayer {
    // Opens the window and starts playing `url`. Errors only cover the window and the engine;
    // a media that cannot be played is reported later through `status().failed`.
    pub fn open(url: String, start: StartAt, options: PlayerOptions) -> Result<Self> {
        let shared = Arc::new(Shared { status: Mutex::new(PlaybackStatus::default()), closed: AtomicBool::new(false) });
        let (commands, command_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("rustplay-video".into())
            .spawn(move || {
                match Worker::new(&url, start, options, Arc::clone(&worker_shared)) {
                    Ok(mut worker) => {
                        let _ = init_tx.send(Ok(()));
                        worker.run(&command_rx);
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e.to_string()));
                    }
                }
                worker_shared.closed.store(true, Ordering::Release);
            })
            .map_err(|e| Error::Media(format!("no se pudo crear el hilo del reproductor: {e}")))?;
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { shared, commands, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(Error::Media(e))
            }
            Err(_) => Err(Error::Media("el hilo del reproductor terminó".into())),
        }
    }

    pub fn status(&self) -> PlaybackStatus {
        let mut status = *self.shared.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        status.closed = self.shared.closed.load(Ordering::Acquire);
        status
    }

    pub fn command(&self, command: PlayerCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        let _ = self.commands.send(PlayerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// State shared between the window procedure and the worker (same thread, but the procedure only
// gets a raw pointer).
struct Ui {
    closed: AtomicBool,
    width: AtomicU32,
    height: AtomicU32,
    fullscreen: AtomicBool,
    controls: AtomicBool,
    visible_until: AtomicU64,
    pointer: AtomicU64,
    // Index + 1 of the button being pressed.
    pressed: AtomicU32,
    seeking: AtomicBool,
    seek_fraction: AtomicU32,
    dirty: AtomicBool,
    actions: AtomicU32,
    video_width: AtomicI32,
    video_height: AtomicI32,
    // Optional buttons of the current media (`layout::Features` bits).
    features: AtomicU32,
    // The open flyout menu (`Menu::code`, 0 when closed), its row count, longest label, scroll
    // offset and the row under the pointer (-1 for none).
    menu: AtomicU32,
    menu_count: AtomicU32,
    menu_chars: AtomicU32,
    menu_scroll: AtomicU32,
    menu_hover: AtomicI32,
    // A picked row: menu code << 16 | row + 1.
    picked: AtomicU32,
    volume_drag: AtomicBool,
    // Volume chosen with the slider (`f32` bits).
    volume_set: AtomicU32,
    // Steps of 5 % from the wheel and the arrow keys, seconds to seek, speed steps.
    volume_steps: AtomicI32,
    seek_seconds: AtomicI32,
    speed_steps: AtomicI32,
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

impl Ui {
    fn pointer(&self) -> Option<(f32, f32)> {
        let p = self.pointer.load(Ordering::Acquire);
        (p != NO_POINTER).then_some(((p >> 32) as u32 as i32 as f32, p as u32 as i32 as f32))
    }

    fn layout(&self, scale: f32) -> layout::Layout {
        layout::layout(
            self.width.load(Ordering::Acquire) as f32,
            self.height.load(Ordering::Acquire) as f32,
            scale,
            Features::from_bits(self.features.load(Ordering::Acquire)),
        )
    }

    // The open menu and where it is.
    fn menu_layout(&self, scale: f32) -> Option<(Menu, layout::MenuLayout)> {
        let menu = Menu::from_code(self.menu.load(Ordering::Acquire))?;
        let l = self.layout(scale);
        let anchor = l.slot_of(menu.button())?.area;
        let ml = layout::menu(
            &l,
            &anchor,
            self.menu_count.load(Ordering::Acquire) as usize,
            self.menu_chars.load(Ordering::Acquire) as usize,
            self.menu_scroll.load(Ordering::Acquire) as usize,
            scale,
            self.width.load(Ordering::Acquire) as f32,
        );
        Some((menu, ml))
    }

    // Keeps the controls on screen for a while (after a key or the wheel).
    fn show_controls(&self) {
        self.visible_until.store(now_ms() + HIDE_AFTER_MS, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
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
            || self.seeking.load(Ordering::Acquire)
            || self.volume_drag.load(Ordering::Acquire)
            || self.menu.load(Ordering::Acquire) != 0
            || self.pointer().is_some_and(|(x, y)| self.layout(scale).over_controls(x, y))
    }

    fn request(&self, action: u32) {
        self.actions.fetch_or(action, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
    }
}

struct WndState {
    ui: Arc<Ui>,
    fullscreen: bool,
    tracking: bool,
    style: u32,
    rect: RECT,
}

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
        let ui = Arc::clone(&state.ui);
        match msg {
            WM_SIZE => {
                if wp.0 != SIZE_MINIMIZED as usize {
                    let l = lp.0 as usize;
                    ui.width.store((l & 0xFFFF) as u32, Ordering::Release);
                    ui.height.store(((l >> 16) & 0xFFFF) as u32, Ordering::Release);
                    ui.dirty.store(true, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lp.0 as u32 as i16 as i32, (lp.0 as u32 >> 16) as i16 as i32);
                let packed = ((x as u32 as u64) << 32) | u64::from(y as u32);
                if ui.pointer.swap(packed, Ordering::AcqRel) != packed {
                    if !state.tracking {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        state.tracking = TrackMouseEvent(&mut tme).is_ok();
                    }
                    if ui.seeking.load(Ordering::Acquire) {
                        let f = ui.layout(dpi_scale(hwnd)).seek_fraction(x as f32);
                        ui.seek_fraction.store(f.to_bits(), Ordering::Release);
                    }
                    if ui.volume_drag.load(Ordering::Acquire) {
                        let f = ui.layout(dpi_scale(hwnd)).volume_fraction(x as f32);
                        ui.volume_set.store(f.to_bits(), Ordering::Release);
                        ui.request(ACTION_VOLUME_SET);
                    }
                    if let Some((_, ml)) = ui.menu_layout(dpi_scale(hwnd)) {
                        let row = ml.hit(x as f32, y as f32).map_or(-1, |r| r as i32);
                        ui.menu_hover.store(row, Ordering::Release);
                    }
                    ui.visible_until.store(now_ms() + HIDE_AFTER_MS, Ordering::Release);
                    ui.dirty.store(true, Ordering::Release);
                    SetTimer(Some(hwnd), TIMER_HIDE, (HIDE_AFTER_MS + 50) as u32, None);
                    SetCursor(LoadCursorW(None, IDC_ARROW).ok());
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                state.tracking = false;
                ui.pointer.store(NO_POINTER, Ordering::Release);
                ui.visible_until.store(0, Ordering::Release);
                ui.dirty.store(true, Ordering::Release);
                LRESULT(0)
            }
            WM_TIMER if wp.0 == TIMER_HIDE => {
                let _ = KillTimer(Some(hwnd), TIMER_HIDE);
                ui.dirty.store(true, Ordering::Release);
                if state.fullscreen && !ui.overlay_wanted(dpi_scale(hwnd)) {
                    SetCursor(None);
                }
                LRESULT(0)
            }
            WM_SETCURSOR
                if (lp.0 & 0xFFFF) as u32 == HTCLIENT && state.fullscreen && !ui.overlay_wanted(dpi_scale(hwnd)) =>
            {
                SetCursor(None);
                LRESULT(1)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let scale = dpi_scale(hwnd);
                let visible = ui.overlay_wanted(scale);
                let pointer = ui.pointer();
                let l = ui.layout(scale);
                // An open menu takes the click first: a row picks, its panel absorbs, a button
                // that opens a menu falls through (it toggles on release), anything else closes it.
                if let (Some((menu, ml)), Some((x, y))) = (ui.menu_layout(scale), pointer) {
                    if let Some(row) = ml.hit(x, y) {
                        ui.picked.store((menu.code() << 16) | (row as u32 + 1), Ordering::Release);
                        ui.menu.store(0, Ordering::Release);
                        ui.request(ACTION_PICK);
                        return LRESULT(0);
                    } else if ml.panel.contains(x, y) {
                        return LRESULT(0);
                    } else if !l.hit(x, y).is_some_and(|i| l.slots()[i].button.menu().is_some()) {
                        ui.menu.store(0, Ordering::Release);
                        ui.dirty.store(true, Ordering::Release);
                        return LRESULT(0);
                    }
                }
                match ui.hovered(scale) {
                    Some(i) if visible => {
                        ui.pressed.store(i as u32 + 1, Ordering::Release);
                        ui.dirty.store(true, Ordering::Release);
                        SetCapture(hwnd);
                    }
                    None if visible && pointer.is_some_and(|(x, y)| l.volume.contains(x, y)) => {
                        if let Some((x, _)) = pointer {
                            ui.volume_set.store(l.volume_fraction(x).to_bits(), Ordering::Release);
                        }
                        ui.volume_drag.store(true, Ordering::Release);
                        ui.request(ACTION_VOLUME_SET);
                        SetCapture(hwnd);
                    }
                    None if visible && pointer.is_some_and(|(x, y)| l.seek.contains(x, y)) => {
                        if let Some((x, _)) = pointer {
                            ui.seek_fraction.store(l.seek_fraction(x).to_bits(), Ordering::Release);
                        }
                        ui.seeking.store(true, Ordering::Release);
                        ui.dirty.store(true, Ordering::Release);
                        SetCapture(hwnd);
                    }
                    None if !pointer.is_some_and(|(x, y)| l.over_controls(x, y)) => {
                        if msg == WM_LBUTTONDBLCLK {
                            toggle_fullscreen(hwnd, state);
                        } else {
                            ui.request(ACTION_PLAY_PAUSE);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                if ui.seeking.swap(false, Ordering::AcqRel) {
                    let _ = ReleaseCapture();
                    ui.request(ACTION_SEEK);
                }
                if ui.volume_drag.swap(false, Ordering::AcqRel) {
                    let _ = ReleaseCapture();
                    ui.dirty.store(true, Ordering::Release);
                }
                let pressed = ui.pressed.swap(0, Ordering::AcqRel);
                if pressed != 0 {
                    let _ = ReleaseCapture();
                    ui.dirty.store(true, Ordering::Release);
                    let index = pressed as usize - 1;
                    let scale = dpi_scale(hwnd);
                    if ui.hovered(scale) == Some(index) {
                        let button = ui.layout(scale).slots().get(index).map(|s| s.button);
                        match button {
                            Some(Button::Volume) => ui.request(ACTION_MUTE),
                            Some(Button::PlayPause) => ui.request(ACTION_PLAY_PAUSE),
                            Some(Button::Fullscreen) => toggle_fullscreen(hwnd, state),
                            Some(Button::Stop) => ui.request(ACTION_STOP),
                            Some(other) => {
                                if let Some(menu) = other.menu() {
                                    let open = ui.menu.load(Ordering::Acquire) == menu.code();
                                    ui.menu.store(if open { 0 } else { menu.code() }, Ordering::Release);
                                    ui.menu_scroll.store(0, Ordering::Release);
                                    ui.menu_hover.store(-1, Ordering::Release);
                                }
                            }
                            None => {}
                        }
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = i32::from((wp.0 >> 16) as u16 as i16);
                let steps = if delta.abs() >= 120 { delta / 120 } else { delta.signum() };
                let scale = dpi_scale(hwnd);
                let over_menu = ui.menu_layout(scale).zip(ui.pointer()).is_some_and(|((_, ml), (x, y))| ml.panel.contains(x, y));
                if over_menu {
                    // A long menu scrolls three rows per notch.
                    let scroll = (ui.menu_scroll.load(Ordering::Acquire) as i32 - steps * 3).max(0);
                    ui.menu_scroll.store(scroll as u32, Ordering::Release);
                    ui.dirty.store(true, Ordering::Release);
                } else if steps != 0 {
                    ui.volume_steps.fetch_add(steps, Ordering::AcqRel);
                    ui.request(ACTION_VOLUME_STEP);
                    ui.show_controls();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let first_press = lp.0 & (1 << 30) == 0;
                match wp.0 {
                    // Arrow keys repeat.
                    VK_UP | VK_DOWN => {
                        ui.volume_steps.fetch_add(if wp.0 == VK_UP { 1 } else { -1 }, Ordering::AcqRel);
                        ui.request(ACTION_VOLUME_STEP);
                        ui.show_controls();
                    }
                    VK_LEFT | VK_RIGHT => {
                        let step = if wp.0 == VK_RIGHT { SEEK_STEP_SECONDS } else { -SEEK_STEP_SECONDS };
                        ui.seek_seconds.fetch_add(step, Ordering::AcqRel);
                        ui.request(ACTION_SEEK_RELATIVE);
                        ui.show_controls();
                    }
                    VK_F11 | VK_F if first_press => toggle_fullscreen(hwnd, state),
                    VK_ESCAPE if first_press && ui.menu.swap(0, Ordering::AcqRel) != 0 => ui.dirty.store(true, Ordering::Release),
                    VK_ESCAPE if first_press && state.fullscreen => toggle_fullscreen(hwnd, state),
                    VK_SPACE if first_press => ui.request(ACTION_PLAY_PAUSE),
                    VK_M if first_press => {
                        ui.request(ACTION_MUTE);
                        ui.show_controls();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CHAR => {
                // `<` and `>` whatever the keyboard layout.
                match wp.0 {
                    0x3C => ui.speed_steps.fetch_sub(1, Ordering::AcqRel),
                    0x3E => ui.speed_steps.fetch_add(1, Ordering::AcqRel),
                    _ => return LRESULT(0),
                };
                ui.request(ACTION_SPEED_STEP);
                ui.show_controls();
                LRESULT(0)
            }
            WM_SYSKEYDOWN if wp.0 == VK_RETURN && lp.0 & (1 << 30) == 0 => {
                toggle_fullscreen(hwnd, state);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_CLOSE => {
                ui.closed.store(true, Ordering::Release);
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => {
                ui.closed.store(true, Ordering::Release);
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

unsafe fn toggle_fullscreen(hwnd: HWND, st: &mut WndState) {
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo.
    unsafe {
        if !st.fullscreen {
            st.style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            let _ = GetWindowRect(hwnd, &mut st.rect);
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
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
        }
        st.ui.fullscreen.store(st.fullscreen, Ordering::Release);
        st.ui.dirty.store(true, Ordering::Release);
    }
}

// Sizes the window to the video's aspect ratio, as large as fits in most of the work area
// without upscaling.
unsafe fn fit_to_video(hwnd: HWND, (vw, vh): (f64, f64)) {
    // SAFETY: llamadas Win32 sobre la ventana propia desde su hilo.
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork;
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let mut rc = RECT::default();
        let _ = AdjustWindowRectEx(&mut rc, style, false, WINDOW_EX_STYLE(0));
        let (ew, eh) = (rc.right - rc.left, rc.bottom - rc.top);
        let max_w = f64::from(work.right - work.left - ew) * FIT_FRACTION;
        let max_h = f64::from(work.bottom - work.top - eh) * FIT_FRACTION;
        let scale = (max_w / vw).min(max_h / vh).min(1.0);
        let w = (vw * scale).round() as i32 + ew;
        let h = (vh * scale).round() as i32 + eh;
        let mut current = RECT::default();
        let _ = GetWindowRect(hwnd, &mut current);
        let cx = (current.left + current.right) / 2;
        let cy = (current.top + current.bottom) / 2;
        let x = (cx - w / 2).clamp(work.left, (work.right - w).max(work.left));
        let y = (cy - h / 2).clamp(work.top, (work.bottom - h).max(work.top));
        let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
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

fn create_window(title: &str, ui: &Arc<Ui>) -> Result<HWND> {
    // SAFETY: registro de clase y creación de ventana en el hilo propietario.
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = w!("RustPlayVideoWindow");
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
            ui: Arc::clone(ui),
            fullscreen: false,
            tracking: false,
            style: 0,
            rect: RECT::default(),
        }));
        let mut rc = RECT { left: 0, top: 0, right: 1280, bottom: 720 };
        let _ = AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(hinst.into()),
            Some(state as *const c_void),
        );
        match created {
            Ok(hwnd) => Ok(hwnd),
            Err(e) => {
                drop(Box::from_raw(state));
                Err(Error::Media(format!("no se pudo crear la ventana de vídeo: {e}")))
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct OverlayKey {
    alpha: f32,
    hovered: Option<usize>,
    pressed: bool,
    fullscreen: bool,
    playing: bool,
    second: u64,
    seek: u32,
    // Volume in percent, plus 1000 when muted.
    volume: u32,
    // Playback speed times 100.
    rate: u32,
    features: u32,
    menu: u32,
    menu_hover: i32,
    menu_scroll: u32,
    menu_sig: u64,
    // Hash of the subtitle on screen, 0 for none.
    cue: u64,
}

struct Fonts {
    scale: f32,
    icon: IDWriteTextFormat,
    label: IDWriteTextFormat,
    item: IDWriteTextFormat,
    // Subtitle format and the pixel size it was made for.
    subtitle: Option<(i32, IDWriteTextFormat)>,
}

#[derive(Clone)]
struct FontSet {
    icon: IDWriteTextFormat,
    label: IDWriteTextFormat,
    item: IDWriteTextFormat,
}

struct Worker {
    shared: Arc<Shared>,
    ui: Arc<Ui>,
    options: PlayerOptions,
    hwnd: HWND,
    gpu: Arc<GpuDevice>,
    engine: engine::Engine,
    start: Option<StartAt>,
    swap: IDXGISwapChain1,
    buf_size: (u32, u32),
    frame: Option<(ID3D11Texture2D, (u32, u32))>,
    d2d: Option<(ID2D1DeviceContext, IDWriteFactory, Option<Fonts>)>,
    overlay_alpha: f32,
    overlay_clock: Instant,
    drawn: Option<OverlayKey>,
    status: PlaybackStatus,
    failure_logged: bool,
    stop: bool,
    // The status is read from the engine a few times per second, not on every loop pass.
    status_at: Option<Instant>,
    // The volume the device asked for and the one chosen here; the engine plays their product.
    device_volume: f32,
    local_volume: f32,
    muted: bool,
    rate: f64,
    // The open menu's rows, which of them is current, and a hash of both.
    menu_kind: u32,
    menu_items: Vec<String>,
    menu_selected: usize,
    menu_sig: u64,
    menu_at: Option<Instant>,
    cue: Option<String>,
    cue_hash: u64,
    live_window: Option<(f64, f64)>,
    // Where the last keyboard seek went, so quick presses add up before the position catches up.
    seek_target: Option<(f64, Instant)>,
}

type Built = (engine::Engine, IDXGISwapChain1, (u32, u32));

impl Worker {
    fn new(url: &str, start: StartAt, options: PlayerOptions, shared: Arc<Shared>) -> Result<Self> {
        super::mf::ensure_started()?;
        let gpu = GpuDevice::create()?;
        let ui = Arc::new(Ui {
            closed: AtomicBool::new(false),
            width: AtomicU32::new(1280),
            height: AtomicU32::new(720),
            fullscreen: AtomicBool::new(false),
            controls: AtomicBool::new(options.controls),
            visible_until: AtomicU64::new(now_ms() + HIDE_AFTER_MS),
            pointer: AtomicU64::new(NO_POINTER),
            pressed: AtomicU32::new(0),
            seeking: AtomicBool::new(false),
            seek_fraction: AtomicU32::new(0),
            dirty: AtomicBool::new(true),
            actions: AtomicU32::new(0),
            video_width: AtomicI32::new(0),
            video_height: AtomicI32::new(0),
            features: AtomicU32::new(0),
            menu: AtomicU32::new(0),
            menu_count: AtomicU32::new(0),
            menu_chars: AtomicU32::new(0),
            menu_scroll: AtomicU32::new(0),
            menu_hover: AtomicI32::new(-1),
            picked: AtomicU32::new(0),
            volume_drag: AtomicBool::new(false),
            volume_set: AtomicU32::new(1.0f32.to_bits()),
            volume_steps: AtomicI32::new(0),
            seek_seconds: AtomicI32::new(0),
            speed_steps: AtomicI32::new(0),
        });
        let hwnd = create_window(&options.title, &ui)?;
        // SAFETY: ventana válida; el resto son llamadas COM sobre objetos propios.
        let built = unsafe { Self::init(&gpu, hwnd, &ui, url) };
        let (engine, swap, buf_size) = match built {
            Ok(v) => v,
            Err(e) => {
                // SAFETY: ventana propia, creada en este hilo.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(e);
            }
        };
        // SAFETY: ventana propia.
        unsafe {
            if options.always_on_top {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            if options.fullscreen {
                let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(VK_F11), LPARAM(0));
            }
        }
        Ok(Self {
            shared,
            ui,
            options,
            hwnd,
            gpu,
            engine,
            start: Some(start),
            swap,
            buf_size,
            frame: None,
            d2d: None,
            overlay_alpha: 0.0,
            overlay_clock: Instant::now(),
            drawn: None,
            status: PlaybackStatus::default(),
            failure_logged: false,
            stop: false,
            status_at: None,
            device_volume: 1.0,
            local_volume: 1.0,
            muted: false,
            rate: 1.0,
            menu_kind: 0,
            menu_items: Vec::new(),
            menu_selected: usize::MAX,
            menu_sig: 0,
            menu_at: None,
            cue: None,
            cue_hash: 0,
            live_window: None,
            seek_target: None,
        })
    }

    unsafe fn init(gpu: &Arc<GpuDevice>, hwnd: HWND, ui: &Arc<Ui>, url: &str) -> Result<Built> {
        // SAFETY: llamadas COM sobre objetos recién creados.
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc)?;
            let (w, h) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
            ui.width.store(w, Ordering::Release);
            ui.height.store(h, Ordering::Release);
            let dxgi: IDXGIDevice = gpu.device.cast()?;
            let adapter = dxgi.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            let swap = factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let engine = engine::Engine::open(url, gpu)
                .map_err(|e| Error::Media(format!("el reproductor de Windows no puede abrir el vídeo: {e}")))?;
            Ok((engine, swap, (w, h)))
        }
    }

    fn pump_messages(&self) {
        // SAFETY: bucle de mensajes del hilo propietario de la ventana.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn run(&mut self, commands: &mpsc::Receiver<PlayerCommand>) {
        while !self.stop && !self.ui.closed.load(Ordering::Acquire) {
            self.pump_messages();
            self.engine.poll();
            while let Ok(command) = commands.try_recv() {
                self.apply(command);
            }
            let actions = self.ui.actions.swap(0, Ordering::AcqRel);
            self.apply_actions(actions);
            if self.status_at.is_none_or(|at| at.elapsed() >= STATUS_INTERVAL) {
                self.status_at = Some(Instant::now());
                self.update_status();
            }
            self.refresh_menu();
            if let Err(e) = self.render() {
                tracing::debug!("fallo al dibujar el vídeo: {e}");
            }
            std::thread::sleep(LOOP_INTERVAL);
        }
        self.engine.close();
        // SAFETY: cierre de la ventana propia.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        self.pump_messages();
    }

    fn apply(&mut self, command: PlayerCommand) {
        match command {
            PlayerCommand::Play => self.engine.play(),
            PlayerCommand::Pause => self.engine.pause(),
            PlayerCommand::Seek(seconds) => self.engine.seek(seconds),
            PlayerCommand::Volume(volume) => {
                self.device_volume = volume.clamp(0.0, 1.0);
                self.refresh_volume();
            }
            PlayerCommand::Stop => self.stop = true,
        }
    }

    // The engine plays the device's volume times the one chosen in this window.
    fn refresh_volume(&mut self) {
        self.engine.set_volume(self.device_volume * if self.muted { 0.0 } else { self.local_volume });
        self.ui.dirty.store(true, Ordering::Release);
    }

    fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
        self.engine.set_rate(rate);
        self.ui.dirty.store(true, Ordering::Release);
    }

    fn apply_actions(&mut self, actions: u32) {
        if actions & ACTION_STOP != 0 {
            self.stop = true;
        }
        if actions & ACTION_PLAY_PAUSE != 0 {
            let paused = self.engine.paused();
            self.apply(if paused { PlayerCommand::Play } else { PlayerCommand::Pause });
        }
        if actions & ACTION_SEEK != 0 {
            let fraction = f64::from(f32::from_bits(self.ui.seek_fraction.load(Ordering::Acquire)));
            if let Some((low, high)) = self.live_window {
                self.apply(PlayerCommand::Seek(low + fraction * (high - low)));
            } else if self.status.duration.is_finite() && self.status.duration > 0.0 {
                self.apply(PlayerCommand::Seek(fraction * self.status.duration));
            }
        }
        if actions & ACTION_SEEK_RELATIVE != 0 {
            let delta = f64::from(self.ui.seek_seconds.swap(0, Ordering::AcqRel));
            let range = match self.live_window {
                Some(window) => Some(window),
                None if self.status.duration.is_finite() && self.status.duration > 0.0 => Some((0.0, self.status.duration)),
                None => None,
            };
            if let Some((low, high)) = range {
                let from = match self.seek_target {
                    Some((target, at)) if at.elapsed() < Duration::from_millis(1200) => target,
                    _ => self.status.position,
                };
                let target = layout::relative_seek(from, delta, low, high);
                self.seek_target = Some((target, Instant::now()));
                self.apply(PlayerCommand::Seek(target));
            }
        }
        if actions & ACTION_MUTE != 0 {
            self.muted = !self.muted;
            // Unmuting with the slider at zero would stay silent.
            if !self.muted && self.local_volume < 0.01 {
                self.local_volume = 0.5;
            }
            self.refresh_volume();
        }
        if actions & ACTION_VOLUME_SET != 0 {
            let volume = f32::from_bits(self.ui.volume_set.load(Ordering::Acquire)).clamp(0.0, 1.0);
            self.local_volume = (volume * 100.0).round() / 100.0;
            self.muted = false;
            self.refresh_volume();
        }
        if actions & ACTION_VOLUME_STEP != 0 {
            let steps = self.ui.volume_steps.swap(0, Ordering::AcqRel);
            self.local_volume = layout::step_volume(self.local_volume, steps);
            if steps > 0 {
                self.muted = false;
            }
            self.refresh_volume();
        }
        if actions & ACTION_SPEED_STEP != 0 {
            let steps = self.ui.speed_steps.swap(0, Ordering::AcqRel);
            let mut rate = self.rate;
            for _ in 0..steps.unsigned_abs() {
                rate = step_speed(rate, steps > 0);
            }
            if (rate - self.rate).abs() > 1e-9 {
                self.set_rate(rate);
            }
        }
        if actions & ACTION_PICK != 0 {
            let picked = self.ui.picked.swap(0, Ordering::AcqRel);
            if picked & 0xFFFF != 0 {
                self.pick(Menu::from_code(picked >> 16), (picked & 0xFFFF) as usize - 1);
            }
        }
    }

    // Rows of a menu and the current one.
    fn menu_content(&self, menu: Menu) -> (Vec<String>, usize) {
        let texts = &self.options.texts;
        match menu {
            Menu::Speed => {
                let selected = SPEEDS.iter().position(|s| (s - self.rate).abs() < 1e-6).unwrap_or(usize::MAX);
                (SPEEDS.iter().map(|&s| layout::format_rate(s)).collect(), selected)
            }
            Menu::Quality => {
                let levels = self.engine.quality_levels();
                let label = |l: &native::QualityLevel| if l.height > 0 { format!("{}p", l.height) } else { format!("{} kbps", l.bandwidth / 1000) };
                let quality = self.engine.quality();
                let auto = match (quality, levels.get(self.engine.playing_rung())) {
                    (Quality::Auto, Some(current)) => format!("{} \u{00B7} {}", texts.quality_auto, label(current)),
                    _ => texts.quality_auto.clone(),
                };
                let mut items = vec![auto];
                items.extend(levels.iter().rev().map(label));
                let selected = match quality {
                    Quality::Auto => 0,
                    Quality::Fixed(i) => levels.len().saturating_sub(i),
                };
                (items, selected)
            }
            Menu::Subtitles => {
                let mut items = vec![texts.subtitles_off.clone()];
                items.extend(self.engine.subtitle_names());
                (items, self.engine.subtitle().map_or(0, |i| i + 1))
            }
        }
    }

    // Keeps the rows of the open menu up to date (the automatic quality changes by itself).
    fn refresh_menu(&mut self) {
        let code = self.ui.menu.load(Ordering::Acquire);
        let Some(menu) = Menu::from_code(code) else {
            self.menu_kind = 0;
            return;
        };
        if code == self.menu_kind && self.menu_at.is_some_and(|at| at.elapsed() < MENU_REFRESH) {
            return;
        }
        self.menu_kind = code;
        self.menu_at = Some(Instant::now());
        let (items, selected) = self.menu_content(menu);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (&items, selected).hash(&mut hasher);
        let sig = hasher.finish();
        if sig != self.menu_sig {
            self.ui.dirty.store(true, Ordering::Release);
        }
        self.ui.menu_count.store(items.len() as u32, Ordering::Release);
        self.ui.menu_chars.store(items.iter().map(|i| i.chars().count()).max().unwrap_or(0) as u32, Ordering::Release);
        self.menu_items = items;
        self.menu_selected = selected;
        self.menu_sig = sig;
    }

    fn pick(&mut self, menu: Option<Menu>, row: usize) {
        match menu {
            Some(Menu::Speed) => {
                if let Some(&rate) = SPEEDS.get(row) {
                    self.set_rate(rate);
                }
            }
            Some(Menu::Quality) => {
                let count = self.engine.quality_levels().len();
                if row <= count {
                    self.engine.set_quality(if row == 0 { Quality::Auto } else { Quality::Fixed(count - row) });
                }
            }
            Some(Menu::Subtitles) => {
                self.engine.set_subtitle(row.checked_sub(1));
                self.cue = None;
                self.cue_hash = 0;
            }
            None => {}
        }
        self.menu_at = None;
        self.ui.dirty.store(true, Ordering::Release);
    }

    fn update_status(&mut self) {
        let opened = self.engine.opened();
        let mut status = PlaybackStatus { opened, rate: self.rate, ..Default::default() };
        if opened {
            status.duration = self.engine.duration();
            status.position = self.engine.position();
            if let Some(start) = self.start.take() {
                let target = match start {
                    StartAt::Seconds(s) => s,
                    StartAt::Fraction(f) if status.duration.is_finite() => f * status.duration,
                    StartAt::Fraction(_) => 0.0,
                };
                if target > 0.5 {
                    self.engine.seek(target);
                }
            }
            // The size does not change with an HLS quality switch (the engine reports a steady
            // one); the window follows the first one.
            if let Some((vw, vh)) = self.engine.video_size() {
                let first = self.ui.video_width.load(Ordering::Acquire) == 0;
                self.ui.video_width.store(vw as i32, Ordering::Release);
                self.ui.video_height.store(vh as i32, Ordering::Release);
                if first && !self.ui.fullscreen.load(Ordering::Acquire) {
                    // SAFETY: ventana propia, ajustada desde su hilo.
                    unsafe { fit_to_video(self.hwnd, (f64::from(vw), f64::from(vh))) };
                }
            }
            self.live_window = self.engine.live_window();
            let features = Features { quality: self.engine.quality_levels().len() > 1, subtitles: self.engine.subtitle_count() > 0 };
            if self.ui.features.swap(features.bits(), Ordering::AcqRel) != features.bits() {
                self.ui.dirty.store(true, Ordering::Release);
            }
            let cue = self.engine.cue_text();
            if cue != self.cue {
                self.cue_hash = cue.as_ref().map_or(0, |c| {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    c.hash(&mut hasher);
                    hasher.finish() | 1
                });
                self.cue = cue;
                self.ui.dirty.store(true, Ordering::Release);
            }
        }
        status.ended = self.engine.ended();
        status.failed = self.engine.failed();
        status.playing = opened && !self.engine.paused() && !status.ended;
        status.buffering = self.engine.buffering();
        if status.failed && !self.failure_logged {
            self.failure_logged = true;
            tracing::warn!("el reproductor no pudo reproducir el vídeo: {}", self.engine.error());
        }
        self.status = status;
        *self.shared.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }

    fn ensure_d2d(&mut self) -> Result<(ID2D1DeviceContext, IDWriteFactory)> {
        if self.d2d.is_none() {
            // SAFETY: fábrica D2D sobre el dispositivo D3D11 propio.
            unsafe {
                let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None)?;
                let dxgi: IDXGIDevice = self.gpu.device.cast()?;
                let device = factory.CreateDevice(&dxgi)?;
                let dc = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                dc.SetDpi(96.0, 96.0);
                let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
                self.d2d = Some((dc, dwrite, None));
            }
        }
        let (dc, dwrite, _) = self.d2d.as_ref().ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        Ok((dc.clone(), dwrite.clone()))
    }

    fn fonts(&mut self, scale: f32) -> Result<FontSet> {
        let (_, dwrite, fonts) = self.d2d.as_mut().ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        if let Some(f) = fonts.as_ref().filter(|f| f.scale == scale) {
            return Ok(FontSet { icon: f.icon.clone(), label: f.label.clone(), item: f.item.clone() });
        }
        let set = build_fonts(dwrite, scale)?;
        *fonts = Some(Fonts { scale, icon: set.icon.clone(), label: set.label.clone(), item: set.item.clone(), subtitle: None });
        Ok(set)
    }

    // Semi-bold text format for subtitles, `size` pixels tall.
    fn subtitle_font(&mut self, size: f32) -> Result<IDWriteTextFormat> {
        let key = size.round() as i32;
        let (_, dwrite, fonts) = self.d2d.as_mut().ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        let fonts = fonts.as_mut().ok_or_else(|| Error::Media("sin fuentes".into()))?;
        if let Some((k, f)) = &fonts.subtitle
            && *k == key
        {
            return Ok(f.clone());
        }
        let mut collection = None;
        // SAFETY: consulta de la colección de fuentes del sistema y creación de un formato.
        unsafe {
            let _ = dwrite.GetSystemFontCollection(&mut collection, false);
            let family = pick_font(collection.as_ref(), &["Segoe UI Variable Text", "Segoe UI"]);
            let fmt = dwrite.CreateTextFormat(
                &family,
                None::<&IDWriteFontCollection>,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                key as f32,
                w!("es-ES"),
            )?;
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fonts.subtitle = Some((key, fmt.clone()));
            Ok(fmt)
        }
    }

    fn overlay_key(&mut self) -> OverlayKey {
        let now = Instant::now();
        let dt = now.duration_since(self.overlay_clock).as_secs_f32().min(0.1);
        self.overlay_clock = now;
        let scale = dpi_scale(self.hwnd);
        let step = dt / FADE_SECONDS;
        self.overlay_alpha = if self.ui.overlay_wanted(scale) {
            (self.overlay_alpha + step).min(1.0)
        } else {
            (self.overlay_alpha - step).max(0.0)
        };
        let menu = self.ui.menu.load(Ordering::Acquire);
        OverlayKey {
            alpha: self.overlay_alpha,
            hovered: self.ui.hovered(scale),
            pressed: self.ui.pressed.load(Ordering::Acquire) != 0,
            fullscreen: self.ui.fullscreen.load(Ordering::Acquire),
            playing: self.status.playing,
            second: if self.overlay_alpha > 0.0 { self.status.position.max(0.0) as u64 } else { 0 },
            seek: if self.ui.seeking.load(Ordering::Acquire) {
                self.ui.seek_fraction.load(Ordering::Acquire)
            } else {
                u32::MAX
            },
            volume: layout::percent(self.local_volume) + if self.muted { 1000 } else { 0 },
            rate: (self.rate * 100.0).round() as u32,
            features: self.ui.features.load(Ordering::Acquire),
            menu,
            menu_hover: if menu != 0 { self.ui.menu_hover.load(Ordering::Acquire) } else { -1 },
            menu_scroll: if menu != 0 { self.ui.menu_scroll.load(Ordering::Acquire) } else { 0 },
            menu_sig: if menu != 0 { self.menu_sig } else { 0 },
            cue: self.cue_hash,
        }
    }

    fn ensure_frame_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some((tex, size)) = &self.frame
            && *size == self.buf_size
        {
            return Ok(tex.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.buf_size.0,
            Height: self.buf_size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // SAFETY: creación de una textura y una vista con descripciones válidas.
        unsafe {
            let mut tex = None;
            self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.ok_or_else(|| Error::Media("sin textura de vídeo".into()))?;
            let mut rtv = None;
            self.gpu.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
            if let Some(rtv) = rtv {
                self.gpu.context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
            self.frame = Some((tex.clone(), self.buf_size));
            Ok(tex)
        }
    }

    fn handle_resize(&mut self) -> Result<bool> {
        let w = self.ui.width.load(Ordering::Acquire);
        let h = self.ui.height.load(Ordering::Acquire);
        if w == 0 || h == 0 {
            return Ok(false);
        }
        if (w, h) != self.buf_size {
            self.frame = None;
            // SAFETY: no quedan vistas ni referencias al búfer trasero.
            unsafe { self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))? };
            self.buf_size = (w, h);
            self.drawn = None;
        }
        Ok(true)
    }

    fn render(&mut self) -> Result<()> {
        if !self.handle_resize()? {
            return Ok(());
        }
        let key = self.overlay_key();
        let dirty = self.ui.dirty.swap(false, Ordering::AcqRel);
        let fresh_texture = self.frame.is_none();
        let tex = self.ensure_frame_texture()?;
        let mut new_frame = fresh_texture;
        let (vw, vh) = (self.ui.video_width.load(Ordering::Acquire), self.ui.video_height.load(Ordering::Acquire));
        if self.engine.opened() && vw > 0 && vh > 0 {
            let dst = letterbox(vw as u32, vh as u32, self.buf_size.0, self.buf_size.1);
            let area = (dst.left as f32, dst.top as f32, (dst.right - dst.left) as f32, (dst.bottom - dst.top) as f32);
            if self.engine.copy_frame(&tex, area, self.buf_size, fresh_texture) {
                new_frame = true;
            }
        }
        if !new_frame && !dirty && self.drawn == Some(key) {
            return Ok(());
        }
        // SAFETY: copia entre texturas del mismo tamaño y formato; el búfer trasero se libera
        // antes de Present.
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            self.gpu.context.CopyResource(&back, &tex);
            drop(back);
        }
        self.overlay_pass(key)?;
        self.drawn = Some(key);
        // SAFETY: cadena de intercambio propia con el búfer trasero ya dibujado.
        unsafe { self.swap.Present(1, DXGI_PRESENT(0)).ok()? };
        Ok(())
    }

    fn overlay_pass(&mut self, key: OverlayKey) -> Result<()> {
        let cue = self.cue.clone();
        if key.alpha <= 0.0 && cue.is_none() {
            return Ok(());
        }
        let scale = dpi_scale(self.hwnd);
        let (dc, dwrite) = self.ensure_d2d()?;
        let fonts = self.fonts(scale)?;
        let (bw, bh) = (self.buf_size.0 as f32, self.buf_size.1 as f32);
        let l = layout::layout(bw, bh, scale, Features::from_bits(key.features));
        // The subtitle goes at the bottom of the picture, above the seek bar while the controls show.
        let (vw, vh) = (self.ui.video_width.load(Ordering::Acquire), self.ui.video_height.load(Ordering::Acquire));
        let video = letterbox(vw.max(0) as u32, vh.max(0) as u32, self.buf_size.0, self.buf_size.1);
        let subtitle = match &cue {
            Some(text) => {
                let height = (video.bottom - video.top) as f32;
                let size = (height * 0.045).clamp(18.0 * scale, 54.0 * scale);
                let mut bottom = video.bottom as f32 - height * 0.06;
                if key.alpha > 0.0 {
                    bottom = bottom.min(l.seek.y - 30.0 * scale);
                }
                Some((text.as_str(), self.subtitle_font(size)?, size, bottom, (video.left as f32, (video.right - video.left) as f32)))
            }
            None => None,
        };
        let position = self.status.position;
        let (fraction, time) = match self.live_window {
            Some((low, high)) => {
                let fraction = if high > low { ((position - low) / (high - low)).clamp(0.0, 1.0) as f32 } else { 1.0 };
                let behind = (high - position).max(0.0);
                let live = self.options.texts.live.as_str();
                (fraction, if behind >= 5.0 { format!("-{} \u{00B7} {live}", layout::format_time(behind)) } else { live.to_owned() })
            }
            None => {
                let fraction = if self.status.duration > 0.0 && self.status.duration.is_finite() {
                    (position / self.status.duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                (fraction, format!("{} / {}", layout::format_time(position), layout::format_time(self.status.duration)))
            }
        };
        let fraction = if key.seek != u32::MAX { f32::from_bits(key.seek) } else { fraction };
        let texts = &self.options.texts;
        let muted = key.volume >= 1000;
        let percent = key.volume % 1000;
        let volume_tip = format!("{} {}%", texts.volume, percent);
        let tooltip = if key.menu != 0 {
            None
        } else {
            key.hovered.and_then(|i| l.slots().get(i)).map(|s| match s.button {
                Button::Volume if muted => texts.unmute.as_str(),
                Button::Volume => volume_tip.as_str(),
                Button::PlayPause if key.playing => texts.pause.as_str(),
                Button::PlayPause => texts.play.as_str(),
                Button::Subtitles => texts.subtitles.as_str(),
                Button::Quality => texts.quality.as_str(),
                Button::Speed => texts.speed.as_str(),
                Button::Fullscreen if key.fullscreen => texts.exit_fullscreen.as_str(),
                Button::Fullscreen => texts.fullscreen.as_str(),
                Button::Stop => texts.stop.as_str(),
            })
        };
        let rate_label = layout::format_rate(self.rate);
        let menu_view = Menu::from_code(key.menu).and_then(|menu| {
            let anchor = l.slot_of(menu.button())?.area;
            let chars = self.menu_items.iter().map(|i| i.chars().count()).max().unwrap_or(0);
            let ml = layout::menu(&l, &anchor, self.menu_items.len(), chars, key.menu_scroll as usize, scale, bw);
            Some(MenuView {
                kind: menu,
                layout: ml,
                items: &self.menu_items,
                selected: self.menu_selected,
                hover: usize::try_from(key.menu_hover).ok(),
            })
        });
        let subtitles_on = self.engine.subtitle().is_some();
        // SAFETY: el mapa de bits del búfer trasero se libera (SetTarget(None)) antes de volver.
        unsafe {
            let back: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let target = dc.CreateBitmapFromDxgiSurface(&back, Some(&props))?;
            dc.SetTarget(&target);
            dc.BeginDraw();
            let mut drawn = Ok(());
            if let Some((text, font, size, bottom, (left, width))) = &subtitle {
                drawn = draw_subtitle(&dc, &dwrite, font, text, *size, *bottom, (*left, *width), scale);
            }
            if drawn.is_ok() && key.alpha > 0.0 {
                let view = View {
                    alpha: key.alpha,
                    hovered: key.hovered,
                    pressed: key.pressed,
                    fullscreen: key.fullscreen,
                    playing: key.playing,
                    fraction,
                    time: &time,
                    tooltip,
                    scale,
                    volume: percent as f32 / 100.0,
                    muted,
                    rate_label: &rate_label,
                    rate_changed: (self.rate - 1.0).abs() > 1e-6,
                    subtitles_on,
                    menu: menu_view,
                };
                drawn = draw_controls(&dc, &dwrite, &fonts, &l, &view);
            }
            let ended = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            drawn?;
            ended?;
        }
        Ok(())
    }
}

fn letterbox(vw: u32, vh: u32, ow: u32, oh: u32) -> RECT {
    if vw == 0 || vh == 0 {
        return RECT { left: 0, top: 0, right: ow as i32, bottom: oh as i32 };
    }
    let (dw, dh) = if u64::from(ow) * u64::from(vh) <= u64::from(oh) * u64::from(vw) {
        (ow, (u64::from(ow) * u64::from(vh) / u64::from(vw)) as u32)
    } else {
        ((u64::from(oh) * u64::from(vw) / u64::from(vh)) as u32, oh)
    };
    let (x, y) = ((ow - dw) / 2, (oh - dh) / 2);
    RECT { left: x as i32, top: y as i32, right: (x + dw) as i32, bottom: (y + dh) as i32 }
}

fn build_fonts(dwrite: &IDWriteFactory, scale: f32) -> Result<FontSet> {
    let mut collection = None;
    // SAFETY: consulta de la colección de fuentes del sistema y creación de formatos.
    unsafe {
        let _ = dwrite.GetSystemFontCollection(&mut collection, false);
        let make = |families: &[&str], size: f32, align: DWRITE_TEXT_ALIGNMENT| -> Result<IDWriteTextFormat> {
            let family = pick_font(collection.as_ref(), families);
            let fmt = dwrite.CreateTextFormat(
                &family,
                None::<&IDWriteFontCollection>,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("es-ES"),
            )?;
            fmt.SetTextAlignment(align)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(fmt)
        };
        let icon = make(&["Segoe Fluent Icons", "Segoe MDL2 Assets"], 16.0 * scale, DWRITE_TEXT_ALIGNMENT_CENTER)?;
        let label = make(&["Segoe UI Variable Text", "Segoe UI"], 12.0 * scale, DWRITE_TEXT_ALIGNMENT_CENTER)?;
        let item = make(&["Segoe UI Variable Text", "Segoe UI"], 13.0 * scale, DWRITE_TEXT_ALIGNMENT_LEADING)?;
        Ok(FontSet { icon, label, item })
    }
}

fn pick_font(coll: Option<&IDWriteFontCollection>, names: &[&str]) -> HSTRING {
    if let Some(coll) = coll {
        for name in names {
            let wide = HSTRING::from(*name);
            let (mut index, mut exists) = (0u32, BOOL(0));
            // SAFETY: consulta de solo lectura sobre la colección de fuentes del sistema.
            let found = unsafe { coll.FindFamilyName(&wide, &mut index, &mut exists) };
            if found.is_ok() && exists.as_bool() {
                return wide;
            }
        }
    }
    HSTRING::from(*names.last().unwrap_or(&"Segoe UI"))
}

struct MenuView<'a> {
    kind: Menu,
    layout: layout::MenuLayout,
    items: &'a [String],
    selected: usize,
    hover: Option<usize>,
}

struct View<'a> {
    alpha: f32,
    hovered: Option<usize>,
    pressed: bool,
    fullscreen: bool,
    playing: bool,
    fraction: f32,
    time: &'a str,
    tooltip: Option<&'a str>,
    scale: f32,
    volume: f32,
    muted: bool,
    rate_label: &'a str,
    rate_changed: bool,
    subtitles_on: bool,
    menu: Option<MenuView<'a>>,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(a: &layout::Area) -> D2D_RECT_F {
    D2D_RECT_F { left: a.x, top: a.y, right: a.x + a.w, bottom: a.y + a.h }
}

fn volume_glyph(volume: f32, muted: bool) -> &'static str {
    if muted || volume <= 0.0 {
        "\u{E74F}"
    } else if volume < 0.34 {
        "\u{E993}"
    } else if volume < 0.67 {
        "\u{E994}"
    } else {
        "\u{E995}"
    }
}

// The icon of a button; None for the ones drawn as text or by hand (speed, subtitles).
fn glyph(button: Button, view: &View<'_>) -> Option<&'static str> {
    match button {
        Button::Volume => Some(volume_glyph(view.volume, view.muted)),
        Button::PlayPause if view.playing => Some("\u{E769}"),
        Button::PlayPause => Some("\u{E768}"),
        Button::Quality => Some("\u{E713}"),
        Button::Fullscreen if view.fullscreen => Some("\u{E73F}"),
        Button::Fullscreen => Some("\u{E740}"),
        Button::Stop => Some("\u{E711}"),
        Button::Speed | Button::Subtitles => None,
    }
}

const ACCENT: (f32, f32, f32) = (0.38, 0.62, 1.0);

// A subtitle on a translucent plate, `bottom` pixels from the top of the window, centred in
// the span (left, width).
#[allow(clippy::too_many_arguments)]
unsafe fn draw_subtitle(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    font: &IDWriteTextFormat,
    text: &str,
    size: f32,
    bottom: f32,
    (left, width): (f32, f32),
    scale: f32,
) -> Result<()> {
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let max_width = (width * 0.86).max(120.0 * scale);
        let text_layout = dwrite.CreateTextLayout(&wide, font, max_width, 10_000.0)?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        text_layout.GetMetrics(&mut metrics)?;
        let (pad_x, pad_y) = (size * 0.55, size * 0.3);
        let (w, h) = (metrics.width.ceil() + 2.0 * pad_x, metrics.height.ceil() + 2.0 * pad_y);
        let plate = layout::Area { x: (left + (width - w) / 2.0).round(), y: (bottom - h).max(0.0).round(), w, h };
        let round = D2D1_ROUNDED_RECT { rect: rect(&plate), radiusX: size * 0.3, radiusY: size * 0.3 };
        let fill = dc.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 0.68), None)?;
        dc.FillRoundedRectangle(&round, &fill);
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 1.0), None)?;
        // The layout was measured at `max_width`; the plate is a little wider than the text.
        let text_area = D2D_RECT_F { left: plate.x + pad_x - 1.0, top: plate.y, right: plate.x + plate.w - pad_x + 1.0, bottom: plate.y + plate.h };
        dc.DrawText(&wide, font, &text_area, &ink, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }
    Ok(())
}

// Draws the seek bar, the button pill, the open menu and the tooltip into the target bound to `dc`.
unsafe fn draw_controls(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    fonts: &FontSet,
    l: &layout::Layout,
    view: &View<'_>,
) -> Result<()> {
    let (a, s) = (view.alpha, view.scale);
    let (icon, label) = (&fonts.icon, &fonts.label);
    let text_flags = D2D1_DRAW_TEXT_OPTIONS_NONE;
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, a), None)?;
        let accent_color = color(ACCENT.0, ACCENT.1, ACCENT.2, a);
        let accent = dc.CreateSolidColorBrush(&accent_color, None)?;
        let radius = l.track.h / 2.0;
        let track = D2D1_ROUNDED_RECT { rect: rect(&l.track), radiusX: radius, radiusY: radius };
        let back = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.28 * a), None)?;
        dc.FillRoundedRectangle(&track, &back);
        let mut filled = l.track;
        filled.w *= view.fraction;
        if filled.w > 0.5 {
            let r = D2D1_ROUNDED_RECT { rect: rect(&filled), radiusX: radius, radiusY: radius };
            dc.FillRoundedRectangle(&r, &accent);
        }
        let knob = D2D1_ELLIPSE {
            point: windows_numerics::Vector2 { X: l.track.x + l.track.w * view.fraction, Y: l.track.y + l.track.h / 2.0 },
            radiusX: 7.0 * s,
            radiusY: 7.0 * s,
        };
        dc.FillEllipse(&knob, &ink);
        let wide: Vec<u16> = view.time.encode_utf16().collect();
        let label_area = layout::Area { x: l.seek.x, y: l.seek.y - 22.0 * s, w: l.seek.w, h: 20.0 * s };
        let shadow = dc.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 0.5 * a), None)?;
        let offset = layout::Area { x: label_area.x + 1.0, y: label_area.y + 1.0, ..label_area };
        dc.DrawText(&wide, label, &rect(&offset), &shadow, text_flags, DWRITE_MEASURING_MODE_NATURAL);
        dc.DrawText(&wide, label, &rect(&label_area), &ink, text_flags, DWRITE_MEASURING_MODE_NATURAL);

        let bar = D2D1_ROUNDED_RECT { rect: rect(&l.bar), radiusX: 8.0 * s, radiusY: 8.0 * s };
        let fill = dc.CreateSolidColorBrush(&color(0.11, 0.11, 0.11, 0.86 * a), None)?;
        let edge = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.12 * a), None)?;
        dc.FillRoundedRectangle(&bar, &fill);
        dc.DrawRoundedRectangle(&bar, &edge, 1.0, None);
        for (i, slot) in l.slots().iter().enumerate() {
            let (button, area) = (slot.button, slot.area);
            let open = view.menu.as_ref().is_some_and(|m| m.kind.button() == button);
            if view.hovered == Some(i) || open {
                let highlight = match (button, view.pressed && view.hovered == Some(i)) {
                    (Button::Stop, false) => color(0.77, 0.17, 0.11, 0.95 * a),
                    (Button::Stop, true) => color(0.64, 0.14, 0.09, 0.95 * a),
                    (_, false) => color(1.0, 1.0, 1.0, 0.10 * a),
                    (_, true) => color(1.0, 1.0, 1.0, 0.05 * a),
                };
                let brush = dc.CreateSolidColorBrush(&highlight, None)?;
                let r = D2D1_ROUNDED_RECT { rect: rect(&area), radiusX: 5.0 * s, radiusY: 5.0 * s };
                dc.FillRoundedRectangle(&r, &brush);
            }
            if let Some(g) = glyph(button, view) {
                let icon_text: Vec<u16> = g.encode_utf16().collect();
                dc.DrawText(&icon_text, icon, &rect(&area), &ink, text_flags, DWRITE_MEASURING_MODE_NATURAL);
            } else if button == Button::Speed {
                let text: Vec<u16> = view.rate_label.encode_utf16().collect();
                let brush = if view.rate_changed { &accent } else { &ink };
                dc.DrawText(&text, label, &rect(&area), brush, text_flags, DWRITE_MEASURING_MODE_NATURAL);
            } else {
                // "CC" in a rounded outline, drawn by hand so it needs no icon font; filled while subtitles are on.
                let (bw, bh) = (24.0 * s, 17.0 * s);
                let outline = layout::Area { x: area.x + (area.w - bw) / 2.0, y: area.y + (area.h - bh) / 2.0, w: bw, h: bh };
                let r = D2D1_ROUNDED_RECT { rect: rect(&outline), radiusX: 3.5 * s, radiusY: 3.5 * s };
                let text: Vec<u16> = "CC".encode_utf16().collect();
                if view.subtitles_on {
                    dc.FillRoundedRectangle(&r, &accent);
                    let dark = dc.CreateSolidColorBrush(&color(0.08, 0.08, 0.08, a), None)?;
                    dc.DrawText(&text, label, &rect(&outline), &dark, text_flags, DWRITE_MEASURING_MODE_NATURAL);
                } else {
                    dc.DrawRoundedRectangle(&r, &ink, 1.5 * s, None);
                    dc.DrawText(&text, label, &rect(&outline), &ink, text_flags, DWRITE_MEASURING_MODE_NATURAL);
                }
            }
        }
        // Volume slider.
        let vt = l.volume_track;
        let vr = vt.h / 2.0;
        let vtrack = D2D1_ROUNDED_RECT { rect: rect(&vt), radiusX: vr, radiusY: vr };
        dc.FillRoundedRectangle(&vtrack, &back);
        let level = if view.muted { 0.0 } else { view.volume };
        let mut vfill = vt;
        vfill.w *= level;
        if vfill.w > 0.5 {
            let r = D2D1_ROUNDED_RECT { rect: rect(&vfill), radiusX: vr, radiusY: vr };
            dc.FillRoundedRectangle(&r, &ink);
        }
        let vknob = D2D1_ELLIPSE {
            point: windows_numerics::Vector2 { X: vt.x + vt.w * level, Y: vt.y + vt.h / 2.0 },
            radiusX: 6.0 * s,
            radiusY: 6.0 * s,
        };
        dc.FillEllipse(&vknob, &ink);

        if let Some(menu) = &view.menu {
            draw_menu(dc, fonts, menu, view, &edge)?;
        } else if let (Some(i), Some(text)) = (view.hovered, view.tooltip)
            && !text.is_empty()
            && let Some(slot) = l.slots().get(i)
        {
            let area = slot.area;
            let wide: Vec<u16> = text.encode_utf16().collect();
            let text_layout = dwrite.CreateTextLayout(&wide, label, 10_000.0, 1_000.0)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            text_layout.GetMetrics(&mut metrics)?;
            let (w, h) = (metrics.widthIncludingTrailingWhitespace + 20.0 * s, 28.0 * s);
            let pill = layout::Area {
                x: (area.x + area.w / 2.0 - w / 2.0).round(),
                y: (l.bar.y - 8.0 * s - h).round(),
                w,
                h,
            };
            let rounded = D2D1_ROUNDED_RECT { rect: rect(&pill), radiusX: 5.0 * s, radiusY: 5.0 * s };
            let tip_fill = dc.CreateSolidColorBrush(&color(0.13, 0.13, 0.13, 0.92 * a), None)?;
            dc.FillRoundedRectangle(&rounded, &tip_fill);
            dc.DrawRoundedRectangle(&rounded, &edge, 1.0, None);
            dc.DrawText(&wide, label, &rect(&pill), &ink, text_flags, DWRITE_MEASURING_MODE_NATURAL);
        }
    }
    Ok(())
}

// The flyout list: rows with a check mark on the current one, a highlight under the pointer.
unsafe fn draw_menu(dc: &ID2D1DeviceContext, fonts: &FontSet, menu: &MenuView<'_>, view: &View<'_>, edge: &ID2D1SolidColorBrush) -> Result<()> {
    let (a, s) = (view.alpha, view.scale);
    let ml = &menu.layout;
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        let panel = D2D1_ROUNDED_RECT { rect: rect(&ml.panel), radiusX: 8.0 * s, radiusY: 8.0 * s };
        let fill = dc.CreateSolidColorBrush(&color(0.12, 0.12, 0.12, 0.96 * a), None)?;
        dc.FillRoundedRectangle(&panel, &fill);
        dc.DrawRoundedRectangle(&panel, edge, 1.0, None);
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, a), None)?;
        let accent = dc.CreateSolidColorBrush(&color(ACCENT.0, ACCENT.1, ACCENT.2, a), None)?;
        let hover_fill = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.09 * a), None)?;
        for index in ml.first..ml.first + ml.rows {
            let Some(row) = ml.item(index) else { continue };
            if menu.hover == Some(index) {
                let r = D2D1_ROUNDED_RECT { rect: rect(&row), radiusX: 4.0 * s, radiusY: 4.0 * s };
                dc.FillRoundedRectangle(&r, &hover_fill);
            }
            let current = menu.selected == index;
            if current {
                let bar = layout::Area { x: row.x + 1.0 * s, y: row.y + row.h / 2.0 - 8.0 * s, w: 3.0 * s, h: 16.0 * s };
                let r = D2D1_ROUNDED_RECT { rect: rect(&bar), radiusX: 1.5 * s, radiusY: 1.5 * s };
                dc.FillRoundedRectangle(&r, &accent);
            }
            let text_area = layout::Area { x: row.x + 14.0 * s, y: row.y, w: row.w - 22.0 * s, h: row.h };
            let Some(text) = menu.items.get(index) else { continue };
            let wide: Vec<u16> = text.encode_utf16().collect();
            let brush = if current { &accent } else { &ink };
            dc.DrawText(&wide, &fonts.item, &rect(&text_area), brush, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
        }
        // A thin scroll bar when the list is longer than the panel.
        let total = menu.items.len();
        if total > ml.rows && ml.rows > 0 {
            let track_h = ml.panel.h - 2.0 * 8.0 * s;
            let thumb_h = (track_h * ml.rows as f32 / total as f32).max(16.0 * s);
            let y = ml.panel.y + 8.0 * s + (track_h - thumb_h) * ml.first as f32 / (total - ml.rows) as f32;
            let thumb = layout::Area { x: ml.panel.x + ml.panel.w - 6.0 * s, y, w: 3.0 * s, h: thumb_h };
            let r = D2D1_ROUNDED_RECT { rect: rect(&thumb), radiusX: 1.5 * s, radiusY: 1.5 * s };
            let brush = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.35 * a), None)?;
            dc.FillRoundedRectangle(&r, &brush);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_keeps_the_aspect_ratio() {
        let wide = letterbox(1920, 1080, 1000, 1000);
        assert_eq!((wide.left, wide.right), (0, 1000));
        assert_eq!(wide.bottom - wide.top, 562);
        let tall = letterbox(1080, 1920, 1000, 1000);
        assert_eq!((tall.top, tall.bottom), (0, 1000));
        assert_eq!(tall.right - tall.left, 562);
        let unknown = letterbox(0, 0, 640, 480);
        assert_eq!((unknown.right, unknown.bottom), (640, 480));
    }

    // Draws the controls with an open menu and a subtitle on a dark picture and saves a PNG
    // (path in RUSTPLAY_OVERLAY_PNG) so the look can be checked by eye.
    #[test]
    #[ignore = "needs a GPU; writes a PNG"]
    fn renders_the_controls_to_a_picture() {
        let path = std::env::var("RUSTPLAY_OVERLAY_PNG").expect("RUSTPLAY_OVERLAY_PNG");
        super::super::mf::ensure_started().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (1000u32, 560u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut tex = None;
        // SAFETY: descripción válida y objetos propios.
        unsafe {
            gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
            let tex = tex.unwrap();
            let factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None).unwrap();
            let dxgi: IDXGIDevice = gpu.device.cast().unwrap();
            let dc = factory.CreateDevice(&dxgi).unwrap().CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE).unwrap();
            dc.SetDpi(96.0, 96.0);
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
            let fonts = build_fonts(&dwrite, 1.0).unwrap();
            let surface: IDXGISurface = tex.cast().unwrap();
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_IGNORE },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let target = dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)).unwrap();
            dc.SetTarget(&target);
            dc.BeginDraw();
            dc.Clear(Some(&color(0.25, 0.3, 0.4, 1.0)));
            let l = layout::layout(w as f32, h as f32, 1.0, Features { quality: true, subtitles: true });
            let anchor = l.slot_of(Button::Quality).unwrap().area;
            let items: Vec<String> = ["Automatic \u{00B7} 720p", "1080p", "720p", "480p", "360p"].iter().map(|s| s.to_string()).collect();
            let ml = layout::menu(&l, &anchor, items.len(), 20, 0, 1.0, w as f32);
            let view = View {
                alpha: 1.0,
                hovered: Some(2),
                pressed: false,
                fullscreen: false,
                playing: true,
                fraction: 0.35,
                time: "1:12 / 4:30",
                tooltip: None,
                scale: 1.0,
                volume: 0.6,
                muted: false,
                rate_label: "1.25\u{00D7}",
                rate_changed: true,
                subtitles_on: true,
                menu: Some(MenuView { kind: Menu::Quality, layout: ml, items: &items, selected: 0, hover: Some(2) }),
            };
            draw_controls(&dc, &dwrite, &fonts, &l, &view).unwrap();
            let font = dwrite
                .CreateTextFormat(w!("Segoe UI"), None::<&IDWriteFontCollection>, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, 24.0, w!("es-ES"))
                .unwrap();
            font.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER).unwrap();
            font.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER).unwrap();
            draw_subtitle(&dc, &dwrite, &font, "Esto es un subt\u{00ED}tulo de prueba\ncon dos l\u{00ED}neas", 24.0, 300.0, (0.0, w as f32), 1.0).unwrap();
            dc.EndDraw(None, None).unwrap();
            dc.SetTarget(None::<&ID2D1Image>);
            let image = crate::media::screenshot::read_texture(&gpu, &tex).unwrap();
            crate::media::screenshot::save_png(std::path::Path::new(&path), &image).unwrap();
        }
    }

    #[test]
    fn status_defaults_to_unopened() {
        let s = PlaybackStatus::default();
        assert!(!s.opened && !s.closed && !s.ended && !s.failed);
    }
}
