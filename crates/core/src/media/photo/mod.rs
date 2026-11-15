// Picture viewer for AirPlay photos and slideshows: one window that shows decoded pictures
// fitted on black, cross-fades between them and optionally zooms slowly (Ken Burns). Direct2D
// draws straight into the window's swap chain, with the same small control pill as the video
// player. Everything lives on one dedicated thread: window, message loop and rendering.
// Decoding (WIC) happens elsewhere; `show` only takes finished pictures.
mod decode;
mod layout;

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, Interface, PCWSTR, w};

pub use self::decode::{MAX_SIDE, Picture, decode};
pub use self::layout::DISSOLVE_SECONDS;
use self::layout::{BUTTONS, Button, FADE_SECONDS, HIDE_AFTER_MS};
use super::gpu::GpuDevice;
use super::player::{PlayerOptions, PlayerTexts};
use crate::{Error, Result};

const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;
const VK_F11: usize = 0x7A;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_HIDE: usize = 1;
const NO_POINTER: u64 = u64::MAX;
const FIT_FRACTION: f64 = 0.9;
const LOOP_INTERVAL: Duration = Duration::from_millis(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    // The new picture replaces the old one at once.
    None,
    // Cross-fade.
    Dissolve,
}

// What to display: a decoded picture with the way it arrives.
#[derive(Debug, Clone)]
pub struct Show {
    pub picture: Arc<Picture>,
    pub transition: Transition,
    // Slow zoom lasting this long (the slide's duration), if wanted.
    pub ken_burns: Option<Duration>,
}

// The window options are the video player's ones (title, fullscreen, always on top, controls and
// the already translated tooltips: `fullscreen`, `exit_fullscreen` and `stop` for closing).
pub type PhotoOptions = PlayerOptions;
pub type PhotoTexts = PlayerTexts;

enum Command {
    Show(Show),
    Close,
}

struct Shared {
    closed: AtomicBool,
}

pub struct PhotoViewer {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl PhotoViewer {
    // Opens the (empty, black) window. Errors cover the window and the GPU only.
    pub fn open(options: PhotoOptions) -> Result<Self> {
        let shared = Arc::new(Shared { closed: AtomicBool::new(false) });
        let (commands, command_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("rustplay-photos".into())
            .spawn(move || {
                match Worker::new(options) {
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
            .map_err(|e| Error::Media(format!("no se pudo crear el hilo del visor de fotos: {e}")))?;
        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self { shared, commands, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(Error::Media(e))
            }
            Err(_) => Err(Error::Media("el hilo del visor de fotos terminó".into())),
        }
    }

    pub fn show(&self, show: Show) {
        let _ = self.commands.send(Command::Show(show));
    }

    // The window was closed (by the user or by `close`).
    pub fn closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }
}

impl Drop for PhotoViewer {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Close);
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
    dirty: AtomicBool,
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
        layout::layout(self.width.load(Ordering::Acquire) as f32, self.height.load(Ordering::Acquire) as f32, scale)
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
            || self.pointer().is_some_and(|(x, y)| self.layout(scale).over_controls(x, y))
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
                let over = ui.pointer().is_some_and(|(x, y)| ui.layout(scale).over_controls(x, y));
                match ui.hovered(scale) {
                    Some(i) if visible => {
                        ui.pressed.store(i as u32 + 1, Ordering::Release);
                        ui.dirty.store(true, Ordering::Release);
                        SetCapture(hwnd);
                    }
                    _ if !over && msg == WM_LBUTTONDBLCLK => toggle_fullscreen(hwnd, state),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let pressed = ui.pressed.swap(0, Ordering::AcqRel);
                if pressed != 0 {
                    let _ = ReleaseCapture();
                    ui.dirty.store(true, Ordering::Release);
                    let index = pressed as usize - 1;
                    if ui.hovered(dpi_scale(hwnd)) == Some(index) {
                        match BUTTONS.get(index) {
                            Some(Button::Fullscreen) => toggle_fullscreen(hwnd, state),
                            Some(Button::Close) => ui.closed.store(true, Ordering::Release),
                            None => {}
                        }
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN if lp.0 & (1 << 30) == 0 => {
                match wp.0 {
                    VK_F11 => toggle_fullscreen(hwnd, state),
                    VK_ESCAPE if state.fullscreen => toggle_fullscreen(hwnd, state),
                    VK_ESCAPE => ui.closed.store(true, Ordering::Release),
                    _ => {}
                }
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
            let _ = SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER);
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

// Sizes the window to the first picture's aspect ratio, as large as fits in most of the work
// area without upscaling.
unsafe fn fit_to_picture(hwnd: HWND, (pw, ph): (f64, f64)) {
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
        let scale = (max_w / pw).min(max_h / ph);
        let w = (pw * scale).round() as i32 + ew;
        let h = (ph * scale).round() as i32 + eh;
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
        let class = w!("RustPlayPhotoWindow");
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
                Err(Error::Media(format!("no se pudo crear la ventana de fotos: {e}")))
            }
        }
    }
}

// A picture uploaded to the GPU and how it is being shown.
struct Slide {
    bitmap: ID2D1Bitmap1,
    size: (u32, u32),
    since: Instant,
    transition: Transition,
    ken_burns: Option<Duration>,
    variant: usize,
}

impl Slide {
    fn progress(&self, now: Instant) -> f32 {
        match self.ken_burns {
            Some(total) if !total.is_zero() => (now.duration_since(self.since).as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0),
            Some(_) => 1.0,
            None => 0.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct OverlayKey {
    alpha: f32,
    hovered: Option<usize>,
    pressed: bool,
    fullscreen: bool,
}

struct Fonts {
    scale: f32,
    icon: IDWriteTextFormat,
    label: IDWriteTextFormat,
}

struct Worker {
    ui: Arc<Ui>,
    options: PhotoOptions,
    hwnd: HWND,
    gpu: Arc<GpuDevice>,
    swap: IDXGISwapChain1,
    buf_size: (u32, u32),
    d2d: Option<(ID2D1DeviceContext, IDWriteFactory, Option<Fonts>)>,
    cur: Option<Slide>,
    prev: Option<Slide>,
    shown: usize,
    overlay_alpha: f32,
    overlay_clock: Instant,
    stop: bool,
}

impl Worker {
    fn new(options: PhotoOptions) -> Result<Self> {
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
            dirty: AtomicBool::new(true),
        });
        let hwnd = create_window(&options.title, &ui)?;
        // SAFETY: ventana válida; el resto son llamadas COM sobre objetos propios.
        let built = unsafe { Self::init(&gpu, hwnd, &ui) };
        let (swap, buf_size) = match built {
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
            ui,
            options,
            hwnd,
            gpu,
            swap,
            buf_size,
            d2d: None,
            cur: None,
            prev: None,
            shown: 0,
            overlay_alpha: 0.0,
            overlay_clock: Instant::now(),
            stop: false,
        })
    }

    unsafe fn init(gpu: &Arc<GpuDevice>, hwnd: HWND, ui: &Arc<Ui>) -> Result<(IDXGISwapChain1, (u32, u32))> {
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
            Ok((swap, (w, h)))
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

    fn run(&mut self, commands: &mpsc::Receiver<Command>) {
        while !self.stop && !self.ui.closed.load(Ordering::Acquire) {
            self.pump_messages();
            while let Ok(command) = commands.try_recv() {
                match command {
                    Command::Show(show) => {
                        if let Err(e) = self.show(show) {
                            tracing::warn!("no se pudo mostrar la foto: {e}");
                        }
                    }
                    Command::Close => self.stop = true,
                }
            }
            if let Err(e) = self.render() {
                tracing::debug!("fallo al dibujar la foto: {e}");
            }
            std::thread::sleep(LOOP_INTERVAL);
        }
        // SAFETY: cierre de la ventana propia.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        self.pump_messages();
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

    fn fonts(&mut self, scale: f32) -> Result<(IDWriteTextFormat, IDWriteTextFormat)> {
        let (_, dwrite, fonts) = self.d2d.as_mut().ok_or_else(|| Error::Media("sin contexto D2D".into()))?;
        if let Some(f) = fonts.as_ref().filter(|f| f.scale == scale) {
            return Ok((f.icon.clone(), f.label.clone()));
        }
        let mut collection = None;
        // SAFETY: consulta de la colección de fuentes del sistema y creación de formatos.
        unsafe {
            let _ = dwrite.GetSystemFontCollection(&mut collection, false);
            let make = |families: &[&str], size: f32| -> Result<IDWriteTextFormat> {
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
                fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                Ok(fmt)
            };
            let icon = make(&["Segoe Fluent Icons", "Segoe MDL2 Assets"], 16.0 * scale)?;
            let label = make(&["Segoe UI Variable Text", "Segoe UI"], 12.0 * scale)?;
            *fonts = Some(Fonts { scale, icon: icon.clone(), label: label.clone() });
            Ok((icon, label))
        }
    }

    // Uploads the picture to the GPU and starts showing it.
    fn show(&mut self, show: Show) -> Result<()> {
        let picture = &show.picture;
        if picture.width == 0 || picture.height == 0 || picture.pixels.len() != picture.width as usize * picture.height as usize * 4 {
            return Err(Error::Media("foto con un tamaño incoherente".into()));
        }
        let (dc, _) = self.ensure_d2d()?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        // SAFETY: los píxeles son válidos y tienen el tamaño comprobado arriba.
        let bitmap = unsafe {
            dc.CreateBitmap(
                D2D_SIZE_U { width: picture.width, height: picture.height },
                Some(picture.pixels.as_ptr().cast()),
                picture.width * 4,
                &props,
            )?
        };
        let first = self.cur.is_none();
        self.shown += 1;
        let slide = Slide {
            bitmap,
            size: (picture.width, picture.height),
            since: Instant::now(),
            transition: show.transition,
            ken_burns: show.ken_burns,
            variant: self.shown,
        };
        self.prev = self.cur.take();
        self.cur = Some(slide);
        if first && !self.ui.fullscreen.load(Ordering::Acquire) {
            // SAFETY: ventana propia, ajustada desde su hilo.
            unsafe { fit_to_picture(self.hwnd, (f64::from(picture.width), f64::from(picture.height))) };
        }
        self.ui.dirty.store(true, Ordering::Release);
        Ok(())
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
        OverlayKey {
            alpha: self.overlay_alpha,
            hovered: self.ui.hovered(scale),
            pressed: self.ui.pressed.load(Ordering::Acquire) != 0,
            fullscreen: self.ui.fullscreen.load(Ordering::Acquire),
        }
    }

    fn handle_resize(&mut self) -> Result<bool> {
        let w = self.ui.width.load(Ordering::Acquire);
        let h = self.ui.height.load(Ordering::Acquire);
        if w == 0 || h == 0 {
            return Ok(false);
        }
        if (w, h) != self.buf_size {
            // SAFETY: no quedan vistas ni referencias al búfer trasero.
            unsafe { self.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))? };
            self.buf_size = (w, h);
        }
        Ok(true)
    }

    fn animating(&self, now: Instant) -> bool {
        self.prev.is_some() || self.cur.as_ref().is_some_and(|s| s.ken_burns.is_some() && s.progress(now) < 1.0)
    }

    fn render(&mut self) -> Result<()> {
        if !self.handle_resize()? {
            return Ok(());
        }
        let now = Instant::now();
        let key = self.overlay_key();
        let dirty = self.ui.dirty.swap(false, Ordering::AcqRel);
        let animating = self.animating(now) || key.alpha > 0.0 && key.alpha < 1.0;
        if !dirty && !animating {
            return Ok(());
        }
        let scale = dpi_scale(self.hwnd);
        let (dc, dwrite) = self.ensure_d2d()?;
        let (icon, label) = self.fonts(scale)?;
        let (bw, bh) = (self.buf_size.0 as f32, self.buf_size.1 as f32);
        if let Some(cur) = &self.cur
            && let Some(prev) = &self.prev
            && layout::dissolve_alpha(now.duration_since(cur.since).as_secs_f32(), transition_seconds(cur)) >= 1.0
        {
            let _ = prev;
            self.prev = None;
        }
        let l = layout::layout(bw, bh, scale);
        let texts = &self.options.texts;
        let tooltip = key.hovered.and_then(|i| BUTTONS.get(i)).map(|b| match b {
            Button::Fullscreen if key.fullscreen => texts.exit_fullscreen.as_str(),
            Button::Fullscreen => texts.fullscreen.as_str(),
            Button::Close => texts.stop.as_str(),
        });
        // SAFETY: el mapa de bits del búfer trasero se libera (SetTarget(None)) antes de volver;
        // el búfer se suelta también antes de Present.
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
            dc.Clear(Some(&color(0.0, 0.0, 0.0, 1.0)));
            let drawn = (|| -> Result<()> {
                if let Some(prev) = &self.prev {
                    draw_slide(&dc, prev, 1.0, 1.0, bw, bh, now, true)?;
                }
                if let Some(cur) = &self.cur {
                    let alpha = if self.prev.is_some() {
                        layout::dissolve_alpha(now.duration_since(cur.since).as_secs_f32(), transition_seconds(cur))
                    } else {
                        1.0
                    };
                    draw_slide(&dc, cur, alpha, 1.0, bw, bh, now, false)?;
                }
                if key.alpha > 0.0 {
                    let view = View { alpha: key.alpha, hovered: key.hovered, pressed: key.pressed, fullscreen: key.fullscreen, tooltip, scale };
                    draw_controls(&dc, &dwrite, &icon, &label, &l, &view)?;
                }
                Ok(())
            })();
            let ended = dc.EndDraw(None, None);
            dc.SetTarget(None::<&ID2D1Image>);
            drop(target);
            drop(back);
            drawn?;
            ended?;
            self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}

fn transition_seconds(slide: &Slide) -> f32 {
    match slide.transition {
        Transition::None => 0.0,
        Transition::Dissolve => DISSOLVE_SECONDS,
    }
}

// Draws a slide with its Ken Burns zoom (a slide that is fading out stays at its final zoom).
#[allow(clippy::too_many_arguments)]
unsafe fn draw_slide(
    dc: &ID2D1DeviceContext,
    slide: &Slide,
    alpha: f32,
    _scale: f32,
    bw: f32,
    bh: f32,
    now: Instant,
    finished: bool,
) -> Result<()> {
    let base = layout::fit(slide.size.0, slide.size.1, bw, bh);
    let area = if slide.ken_burns.is_some() {
        layout::ken_burns(base, if finished { 1.0 } else { slide.progress(now) }, slide.variant)
    } else {
        base
    };
    let dest = D2D_RECT_F { left: area.x, top: area.y, right: area.x + area.w, bottom: area.y + area.h };
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        dc.DrawBitmap(&slide.bitmap, Some(&dest), alpha, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
    }
    Ok(())
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

struct View<'a> {
    alpha: f32,
    hovered: Option<usize>,
    pressed: bool,
    fullscreen: bool,
    tooltip: Option<&'a str>,
    scale: f32,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(a: &layout::Area) -> D2D_RECT_F {
    D2D_RECT_F { left: a.x, top: a.y, right: a.x + a.w, bottom: a.y + a.h }
}

fn glyph(button: Button, view: &View<'_>) -> &'static str {
    match button {
        Button::Fullscreen if view.fullscreen => "\u{E73F}",
        Button::Fullscreen => "\u{E740}",
        Button::Close => "\u{E711}",
    }
}

// Draws the button pill and the tooltip into the target bound to `dc`.
unsafe fn draw_controls(
    dc: &ID2D1DeviceContext,
    dwrite: &IDWriteFactory,
    icon: &IDWriteTextFormat,
    label: &IDWriteTextFormat,
    l: &layout::Layout,
    view: &View<'_>,
) -> Result<()> {
    let (a, s) = (view.alpha, view.scale);
    let text_flags = D2D1_DRAW_TEXT_OPTIONS_NONE;
    // SAFETY: dibujo D2D entre BeginDraw y EndDraw del llamador.
    unsafe {
        let ink = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, a), None)?;
        let bar = D2D1_ROUNDED_RECT { rect: rect(&l.bar), radiusX: 8.0 * s, radiusY: 8.0 * s };
        let fill = dc.CreateSolidColorBrush(&color(0.11, 0.11, 0.11, 0.86 * a), None)?;
        let edge = dc.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.12 * a), None)?;
        dc.FillRoundedRectangle(&bar, &fill);
        dc.DrawRoundedRectangle(&bar, &edge, 1.0, None);
        for (i, button) in BUTTONS.iter().enumerate() {
            let area = l.buttons[i];
            if view.hovered == Some(i) {
                let highlight = match (button, view.pressed) {
                    (Button::Close, false) => color(0.77, 0.17, 0.11, 0.95 * a),
                    (Button::Close, true) => color(0.64, 0.14, 0.09, 0.95 * a),
                    (_, false) => color(1.0, 1.0, 1.0, 0.10 * a),
                    (_, true) => color(1.0, 1.0, 1.0, 0.05 * a),
                };
                let brush = dc.CreateSolidColorBrush(&highlight, None)?;
                let r = D2D1_ROUNDED_RECT { rect: rect(&area), radiusX: 5.0 * s, radiusY: 5.0 * s };
                dc.FillRoundedRectangle(&r, &brush);
            }
            let icon_text: Vec<u16> = glyph(*button, view).encode_utf16().collect();
            dc.DrawText(&icon_text, icon, &rect(&area), &ink, text_flags, DWRITE_MEASURING_MODE_NATURAL);
        }
        if let (Some(i), Some(text)) = (view.hovered, view.tooltip)
            && !text.is_empty()
        {
            let area = l.buttons[i];
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

#[cfg(test)]
mod tests {
    use super::*;

    // Opens a real window; skipped where there is no GPU. Never fullscreen, never on top.
    #[test]
    fn viewer_opens_shows_and_closes() {
        let options = PhotoOptions { title: "RustPlay test".into(), controls: true, ..Default::default() };
        let Ok(viewer) = PhotoViewer::open(options) else {
            eprintln!("sin GPU o escritorio: se omite la prueba del visor");
            return;
        };
        let png = decode::tests::png(4, 2, &[10, 20, 30, 255].repeat(8));
        let picture = Arc::new(decode(&png).unwrap());
        viewer.show(Show { picture: Arc::clone(&picture), transition: Transition::None, ken_burns: None });
        viewer.show(Show { picture, transition: Transition::Dissolve, ken_burns: Some(Duration::from_millis(300)) });
        std::thread::sleep(Duration::from_millis(500));
        assert!(!viewer.closed());
        viewer.close();
        let start = Instant::now();
        while !viewer.closed() && start.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(viewer.closed());
    }
}
