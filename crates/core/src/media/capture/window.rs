use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, D3D11_TEXTURE2D_DESC, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::Interface;

use super::{Blit, BlitTarget, CapturedFrame, DesktopDuplicator, MtGuard, POOL_SIZE, qpc_to_100ns};
use crate::media::gpu::GpuDevice;
use crate::{Error, Result};

const FRAME_BUFFERS: i32 = 2;
const ACCESS_WAIT: Duration = Duration::from_millis(500);

// Captures one window with Windows.Graphics.Capture and letterboxes it into a fixed-size NV12
// pool, so resizing the window never changes the encoded resolution.
pub struct WindowCapture {
    pub gpu: Arc<GpuDevice>,
    out_w: u32,
    out_h: u32,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    winrt_device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    frames: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived: Receiver<()>,
    closed: Arc<AtomicBool>,
    tokens: (i64, i64),
    frame_size: SizeInt32,
    blit: Option<Blit>,
    pool: Vec<ID3D11Texture2D>,
    next_slot: usize,
    last: Option<(usize, i64)>,
    last_pts: i64,
    qpc_freq: i64,
    min_interval: Duration,
    last_out: Instant,
}

// SAFETY: como DesktopDuplicator: dispositivo con protección multihilo, uso exclusivo vía `&mut self`;
// los objetos WinRT de captura son ágiles.
unsafe impl Send for WindowCapture {}

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

pub(crate) fn window_exists(raw: isize) -> bool {
    // SAFETY: IsWindow acepta cualquier valor.
    raw != 0 && unsafe { IsWindow(Some(hwnd(raw))) }.as_bool()
}

fn even_size(size: SizeInt32) -> (u32, u32) {
    (size.Width.max(2) as u32, size.Height.max(2) as u32)
}

// Windows 11 asks desktop apps to request capture capabilities; unpackaged apps are granted them
// without a prompt. Earlier systems lack the API and need nothing.
fn request_access(kind: GraphicsCaptureAccessKind) -> Option<i32> {
    let op = GraphicsCaptureAccess::RequestAccessAsync(kind).ok()?;
    let started = Instant::now();
    // AsyncStatus::Started is 0.
    while started.elapsed() < ACCESS_WAIT && op.Status().is_ok_and(|status| status.0 == 0) {
        std::thread::sleep(Duration::from_millis(10));
    }
    op.GetResults().ok().map(|status| status.0)
}

// Asks for borderless capture (Windows 11); older systems keep the yellow border.
fn hide_border(session: &GraphicsCaptureSession) {
    let access = request_access(GraphicsCaptureAccessKind::Borderless);
    match session.SetIsBorderRequired(false) {
        Ok(()) => tracing::debug!(?access, "captura de ventana sin borde amarillo"),
        Err(e) => tracing::debug!(?access, "no se puede ocultar el borde de captura: {e}"),
    }
}

impl WindowCapture {
    pub fn new(gpu: Arc<GpuDevice>, window: isize, out_width: u32, out_height: u32) -> Result<Self> {
        if out_width < 2 || out_height < 2 || out_width % 2 != 0 || out_height % 2 != 0 {
            return Err(Error::Media(format!("el tamaño de salida {out_width}x{out_height} debe ser par (NV12)")));
        }
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(Error::Media("esta versión de Windows no permite capturar ventanas".into()));
        }
        if !window_exists(window) {
            return Err(Error::Media("la ventana elegida ya no existe".into()));
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: HWND comprobado arriba; la interfaz devuelve un objeto WinRT propio.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd(window)) }
            .map_err(|e| Error::Media(format!("no se puede capturar esa ventana: {e}")))?;
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        // SAFETY: envuelve el dispositivo D3D11 propio para WinRT.
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let frame_size = item.Size()?;
        let frames = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_BUFFERS,
            SizeInt32 { Width: frame_size.Width.max(2), Height: frame_size.Height.max(2) },
        )?;
        let (signal, arrived) = sync_channel::<()>(1);
        let closed = Arc::new(AtomicBool::new(false));
        let on_frame = signal.clone();
        let frame_token = frames.FrameArrived(&TypedEventHandler::new(move |_, _| {
            let _ = on_frame.try_send(());
            Ok(())
        }))?;
        let flag = Arc::clone(&closed);
        let closed_token = item.Closed(&TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Release);
            let _ = signal.try_send(());
            Ok(())
        }))?;
        let session = frames.CreateCaptureSession(&item)?;
        hide_border(&session);
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let pool = DesktopDuplicator::create_pool(&gpu.device, out_width, out_height)?;
        let mut freq = 0i64;
        // SAFETY: puntero de salida local.
        unsafe { QueryPerformanceFrequency(&mut freq)? };
        let mut me = Self {
            gpu,
            out_w: out_width,
            out_h: out_height,
            video_device,
            video_context,
            winrt_device,
            item,
            frames,
            session,
            arrived,
            closed,
            tokens: (frame_token, closed_token),
            frame_size,
            blit: None,
            pool,
            next_slot: 0,
            last: None,
            last_pts: 0,
            qpc_freq: freq,
            min_interval: Duration::ZERO,
            last_out: Instant::now(),
        };
        me.prime()?;
        me.session
            .StartCapture()
            .map_err(|e| Error::Media(format!("no se pudo iniciar la captura de la ventana: {e}")))?;
        let name = me.item.DisplayName().map(|n| n.to_string()).unwrap_or_default();
        tracing::info!("capturando la ventana «{name}» ({}x{})", frame_size.Width, frame_size.Height);
        Ok(me)
    }

    pub fn set_draw_cursor(&mut self, draw: bool) {
        if let Err(e) = self.session.SetIsCursorCaptureEnabled(draw) {
            tracing::debug!("no se pudo cambiar el puntero en la captura de ventana: {e}");
        }
    }

    pub fn set_max_fps(&mut self, fps: u32) {
        self.min_interval = if fps == 0 { Duration::ZERO } else { Duration::from_micros(1_000_000 / fps as u64) };
        if fps > 0 {
            // Windows 11 24H2 and later; earlier versions deliver every composition and we skip extras.
            let _ = self.session.SetMinUpdateInterval(TimeSpan { Duration: 10_000_000 / i64::from(fps) });
        }
    }

    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn target(&self) -> BlitTarget<'_> {
        BlitTarget {
            device: &self.gpu.device,
            video_device: &self.video_device,
            video_context: &self.video_context,
            pool: &self.pool,
            out_w: self.out_w,
            out_h: self.out_h,
        }
    }

    fn now_100ns(&self) -> i64 {
        let mut t = 0i64;
        // SAFETY: puntero de salida local.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        qpc_to_100ns(t, self.qpc_freq)
    }

    // Black first frame, so a minimised or slow window still has something to send.
    fn prime(&mut self) -> Result<()> {
        let (w, h) = even_size(self.frame_size);
        let _lock = MtGuard::new(&self.gpu.device);
        let blit = DesktopDuplicator::build_blit_for(&self.target(), w, h, DXGI_FORMAT_B8G8R8A8_UNORM)?;
        // SAFETY: vista de destino propia, contexto bloqueado.
        unsafe { self.gpu.context.ClearRenderTargetView(&blit.composed_rtv, &[0.0, 0.0, 0.0, 1.0]) };
        blit.run(&self.video_context, 0)?;
        self.blit = Some(blit);
        let pts = self.now_100ns();
        self.last = Some((0, pts));
        self.last_pts = pts;
        self.next_slot = 1 % POOL_SIZE;
        Ok(())
    }

    fn latest(&self) -> Option<Direct3D11CaptureFrame> {
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(frame) = self.frames.TryGetNextFrame() {
            if let Some(older) = latest.replace(frame) {
                let _ = older.Close();
            }
        }
        latest
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>> {
        let mut timeout = timeout;
        if !self.min_interval.is_zero() {
            let elapsed = self.last_out.elapsed();
            if elapsed < self.min_interval {
                let rest = self.min_interval - elapsed;
                if rest >= timeout {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
                std::thread::sleep(rest);
                timeout -= rest;
            }
        }
        if self.closed() {
            return Err(Error::Media("la ventana compartida se ha cerrado".into()));
        }
        let frame = match self.latest() {
            Some(frame) => frame,
            None => {
                if self.arrived.recv_timeout(timeout).is_err() {
                    return Ok(None);
                }
                if self.closed() {
                    return Err(Error::Media("la ventana compartida se ha cerrado".into()));
                }
                match self.latest() {
                    Some(frame) => frame,
                    None => return Ok(None),
                }
            }
        };
        let result = self.process(&frame);
        let _ = frame.Close();
        result?;
        self.last_out = Instant::now();
        Ok(self.last_frame())
    }

    fn process(&mut self, frame: &Direct3D11CaptureFrame) -> Result<()> {
        let content = frame.ContentSize()?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: la superficie de un fotograma de captura es una textura D3D11 del mismo dispositivo.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: descripción de una textura válida.
        unsafe { texture.GetDesc(&mut desc) };
        if content.Width != self.frame_size.Width || content.Height != self.frame_size.Height {
            let size = SizeInt32 { Width: content.Width.max(2), Height: content.Height.max(2) };
            tracing::debug!("la ventana compartida cambió de tamaño: {}x{}", size.Width, size.Height);
            self.frames.Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, FRAME_BUFFERS, size)?;
            self.frame_size = content;
        }
        let w = (content.Width.max(2) as u32).min(desc.Width).max(1);
        let h = (content.Height.max(2) as u32).min(desc.Height).max(1);
        let _lock = MtGuard::new(&self.gpu.device);
        let stale = self.blit.as_ref().is_none_or(|b| (b.width, b.height, b.format) != (w, h, desc.Format));
        if stale {
            self.blit = Some(DesktopDuplicator::build_blit_for(&self.target(), w, h, desc.Format)?);
        }
        let Some(blit) = self.blit.as_ref() else { return Ok(()) };
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
        // SAFETY: copia en la GPU entre texturas del mismo dispositivo; la región cabe en ambas.
        unsafe { self.gpu.context.CopySubresourceRegion(&blit.composed, 0, 0, 0, 0, &texture, 0, Some(&region)) };
        let slot = self.next_slot;
        blit.run(&self.video_context, slot)?;
        let mut pts = self.now_100ns();
        if pts <= self.last_pts {
            pts = self.last_pts + 1;
        }
        self.last_pts = pts;
        self.last = Some((slot, pts));
        self.next_slot = (slot + 1) % POOL_SIZE;
        Ok(())
    }

    pub fn last_frame(&self) -> Option<CapturedFrame<'_>> {
        let (slot, pts) = self.last?;
        Some(CapturedFrame { nv12: &self.pool[slot], width: self.out_w, height: self.out_h, pts })
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        let _ = self.frames.RemoveFrameArrived(self.tokens.0);
        let _ = self.item.RemoveClosed(self.tokens.1);
        let _ = self.session.Close();
        let _ = self.frames.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn pump() {
        let mut msg = MSG::default();
        // SAFETY: bombeo de mensajes del hilo de la prueba.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    // Captures a small window created by the test itself; skips when there is no GPU or no
    // Windows.Graphics.Capture (e.g. a service session).
    #[test]
    fn captures_a_window_into_a_fixed_size_frame() {
        let gpu = match GpuDevice::create() {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("prueba omitida: sin GPU ({e})");
                return;
            }
        };
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            eprintln!("prueba omitida: Windows.Graphics.Capture no disponible");
            return;
        }
        // SAFETY: ventana propia de la prueba, destruida al final.
        let window = unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: w!("RustPlayCaptureTest"),
                hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("RustPlayCaptureTest"),
                w!("RustPlay prueba de captura"),
                WS_OVERLAPPEDWINDOW,
                40,
                40,
                320,
                200,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            hwnd
        };
        pump();
        let raw = window.0 as isize;
        let result = (|| -> Result<(u32, u32, bool)> {
            let mut capture = WindowCapture::new(Arc::clone(&gpu), raw, 1280, 720)?;
            capture.set_draw_cursor(false);
            let primed = capture.last_frame().map(|f| (f.width, f.height));
            assert_eq!(primed, Some((1280, 720)), "black first frame");
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut got = false;
            while Instant::now() < deadline && !got {
                pump();
                got = capture.next_frame(Duration::from_millis(50))?.is_some();
            }
            let frame = capture.last_frame().expect("frame");
            Ok((frame.width, frame.height, got))
        })();
        // SAFETY: destruye la ventana de la prueba.
        unsafe {
            let _ = DestroyWindow(window);
        }
        pump();
        match result {
            Ok((w, h, got)) => {
                assert_eq!((w, h), (1280, 720));
                assert!(got, "no window frame arrived within 5 s");
            }
            Err(e) => eprintln!("prueba omitida: no se pudo capturar la ventana ({e})"),
        }
    }
}
