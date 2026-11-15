use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Interface;

use super::gpu::GpuDevice;
use crate::{Error, Result};

mod cursor;
mod source;
mod window;
mod window_list;

use cursor::Cursor;
pub use source::{CaptureSource, Capturer};
pub use window::WindowCapture;
pub use window_list::{WindowCandidate, WindowInfo, filter_windows, list_windows, shareable};

const POOL_SIZE: usize = 3;
// BT.709, YCbCr con rango limitado 16-235 (bits: matriz = 1, rango nominal = 1).
const CS_OUTPUT_BT709_LIMITED: u32 = (1 << 2) | (1 << 4);

pub struct CapturedFrame<'a> {
    pub nv12: &'a ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub pts: i64,
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub adapter: String,
}

fn wide_to_string(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
    // SAFETY: enumeración DXGI de solo lectura con objetos COM locales.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut list = Vec::new();
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let adapter_name = wide_to_string(&adapter.GetDesc1()?.Description);
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let d = output.GetDesc()?;
                let r = d.DesktopCoordinates;
                list.push(MonitorInfo {
                    index: o,
                    name: wide_to_string(&d.DeviceName),
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                    adapter: adapter_name.clone(),
                });
                o += 1;
            }
        }
        Ok(list)
    }
}

fn monitors_text() -> String {
    match list_monitors() {
        Ok(l) if !l.is_empty() => l
            .iter()
            .map(|m| {
                format!(
                    "salida {} \"{}\" {}x{} en el adaptador \"{}\"",
                    m.index, m.name, m.width, m.height, m.adapter
                )
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => "ninguno".into(),
    }
}

// Rectángulo (x, y, ancho, alto) del origen escalado dentro del destino, con dimensiones pares.
pub(crate) fn letterbox(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    let (sw, sh, dw, dh) = (src_w as u64, src_h as u64, dst_w as u64, dst_h as u64);
    let (w, h) = if sw * dh >= sh * dw {
        (dw, (sh * dw / sw) & !1)
    } else {
        ((sw * dh / sh) & !1, dh)
    };
    let (w, h) = (w.max(2).min(dw), h.max(2).min(dh));
    let (x, y) = (((dw - w) / 2) & !1, ((dh - h) / 2) & !1);
    (x as u32, y as u32, w as u32, h as u32)
}

fn qpc_to_100ns(ticks: i64, freq: i64) -> i64 {
    (ticks as i128 * 10_000_000 / freq.max(1) as i128) as i64
}

struct Blit {
    processor: ID3D11VideoProcessor,
    composed: ID3D11Texture2D,
    composed_rtv: ID3D11RenderTargetView,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<ID3D11VideoProcessorOutputView>,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

// What a blit needs from its owner: the device, the video processor interfaces and the NV12 pool.
struct BlitTarget<'a> {
    device: &'a ID3D11Device,
    video_device: &'a ID3D11VideoDevice,
    video_context: &'a ID3D11VideoContext,
    pool: &'a [ID3D11Texture2D],
    out_w: u32,
    out_h: u32,
}

impl Blit {
    // Scales and converts `composed` into the NV12 pool texture `slot`.
    fn run(&self, video_context: &ID3D11VideoContext, slot: usize) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let streams = [stream];
        // SAFETY: objetos propios del mismo dispositivo; el llamador bloquea el contexto.
        let r = unsafe { video_context.VideoProcessorBlt(&self.processor, &self.outputs[slot], 0, &streams) };
        let [s] = streams;
        drop(ManuallyDrop::into_inner(s.pInputSurface));
        r.map_err(Into::into)
    }
}

pub(crate) struct MtGuard(ID3D11Multithread);

impl MtGuard {
    pub(crate) fn new(device: &ID3D11Device) -> Option<Self> {
        let mt: ID3D11Multithread = device.cast().ok()?;
        // SAFETY: bloqueo del contexto inmediato liberado en Drop.
        unsafe { mt.Enter() };
        Some(Self(mt))
    }
}

impl Drop for MtGuard {
    fn drop(&mut self) {
        // SAFETY: empareja el Enter de `new`.
        unsafe { self.0.Leave() };
    }
}

pub struct DesktopDuplicator {
    pub gpu: Arc<GpuDevice>,
    pub monitor: u32,
    out_w: u32,
    out_h: u32,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    duplication: Option<IDXGIOutputDuplication>,
    blit: Option<Blit>,
    pool: Vec<ID3D11Texture2D>,
    next_slot: usize,
    last: Option<(usize, i64)>,
    last_pts: i64,
    native: (u32, u32),
    cursor: Cursor,
    draw_cursor: bool,
    shape_buf: Vec<u8>,
    qpc_freq: i64,
    min_interval: Duration,
    last_out: Instant,
    warned_lost: bool,
}

// SAFETY: los objetos COM D3D11/DXGI pertenecen a un dispositivo con protección multihilo;
// `&mut self` garantiza uso exclusivo desde un único hilo a la vez.
unsafe impl Send for DesktopDuplicator {}

impl DesktopDuplicator {
    pub fn new(gpu: Arc<GpuDevice>, monitor: u32, out_width: u32, out_height: u32) -> Result<Self> {
        if out_width < 2 || out_height < 2 || out_width % 2 != 0 || out_height % 2 != 0 {
            return Err(Error::Media(format!(
                "el tamaño de salida {out_width}x{out_height} debe ser par (NV12)"
            )));
        }
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let cursor = Cursor::new(&gpu.device)?;
        let mut freq = 0i64;
        // SAFETY: puntero de salida local.
        unsafe { QueryPerformanceFrequency(&mut freq)? };
        let pool = Self::create_pool(&gpu.device, out_width, out_height)?;
        let mut me = Self {
            gpu,
            monitor,
            out_w: out_width,
            out_h: out_height,
            video_device,
            video_context,
            duplication: None,
            blit: None,
            pool,
            next_slot: 0,
            last: None,
            last_pts: 0,
            native: (0, 0),
            cursor,
            draw_cursor: true,
            shape_buf: Vec::new(),
            qpc_freq: freq,
            min_interval: Duration::ZERO,
            last_out: Instant::now(),
            warned_lost: false,
        };
        me.open_duplication()?;
        Ok(me)
    }

    pub fn native_size(&self) -> (u32, u32) {
        self.native
    }

    // Limita la cadencia de salida; las actualizaciones intermedias se acumulan sin perderse.
    pub fn set_draw_cursor(&mut self, draw: bool) {
        self.draw_cursor = draw;
    }

    pub fn set_max_fps(&mut self, fps: u32) {
        self.min_interval = if fps == 0 {
            Duration::ZERO
        } else {
            Duration::from_micros(1_000_000 / fps as u64)
        };
    }

    fn create_pool(device: &ID3D11Device, w: u32, h: u32) -> Result<Vec<ID3D11Texture2D>> {
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
            ..Default::default()
        };
        // SAFETY: descripciones válidas y punteros de salida locales.
        unsafe {
            let mut probe = None;
            if device
                .CreateTexture2D(&desc, None, Some(&mut probe))
                .is_err()
            {
                desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
                tracing::warn!(
                    "la GPU no admite D3D11_BIND_VIDEO_ENCODER en NV12: se usa solo RENDER_TARGET"
                );
            }
            let mut pool = Vec::with_capacity(POOL_SIZE);
            for _ in 0..POOL_SIZE {
                let mut tex = None;
                device.CreateTexture2D(&desc, None, Some(&mut tex))?;
                pool.push(
                    tex.ok_or_else(|| Error::Media("no se pudo crear la textura NV12".into()))?,
                );
            }
            Ok(pool)
        }
    }

    fn open_duplication(&mut self) -> Result<()> {
        // SAFETY: llamadas DXGI sobre el adaptador del dispositivo propio.
        let dup = unsafe {
            let dxgi: IDXGIDevice = self.gpu.device.cast()?;
            let adapter = dxgi.GetAdapter()?;
            let output = adapter.EnumOutputs(self.monitor).map_err(|_| {
                Error::Media(format!(
                    "el monitor {} no existe en el adaptador de la GPU usada; disponibles: {}",
                    self.monitor,
                    monitors_text()
                ))
            })?;
            let output1: IDXGIOutput1 = output.cast()?;
            output1.DuplicateOutput(&self.gpu.device).map_err(|e| {
                Error::Media(format!(
                    "no se pudo duplicar la salida {} ({e}); monitores: {}",
                    self.monitor,
                    monitors_text()
                ))
            })?
        };
        // SAFETY: descripción de solo lectura de la duplicación recién creada.
        let desc = unsafe { dup.GetDesc() };
        let (w, h, fmt) = (
            desc.ModeDesc.Width,
            desc.ModeDesc.Height,
            desc.ModeDesc.Format,
        );
        let rebuild = self
            .blit
            .as_ref()
            .is_none_or(|b| (b.width, b.height, b.format) != (w, h, fmt));
        if rebuild {
            self.blit = Some(self.build_blit(w, h, fmt)?);
        }
        self.native = (w, h);
        self.duplication = Some(dup);
        Ok(())
    }

    fn build_blit(&self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<Blit> {
        let target = BlitTarget {
            device: &self.gpu.device,
            video_device: &self.video_device,
            video_context: &self.video_context,
            pool: &self.pool,
            out_w: self.out_w,
            out_h: self.out_h,
        };
        Self::build_blit_for(&target, w, h, format)
    }

    fn build_blit_for(target: &BlitTarget<'_>, w: u32, h: u32, format: DXGI_FORMAT) -> Result<Blit> {
        let device = target.device;
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: w,
            InputHeight: h,
            OutputFrameRate: rate,
            OutputWidth: target.out_w,
            OutputHeight: target.out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let (mut composed, mut rtv, mut input) = (None, None, None);
        let missing = || Error::Media("no se pudo crear el procesador de vídeo".into());
        // SAFETY: descripciones válidas, punteros de salida locales y objetos propios.
        unsafe {
            let enumerator = target.video_device.CreateVideoProcessorEnumerator(&content)?;
            let support = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
            if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                return Err(Error::Media(
                    "la GPU no admite salida NV12 en el procesador de vídeo".into(),
                ));
            }
            device.CreateTexture2D(&tex_desc, None, Some(&mut composed))?;
            let composed: ID3D11Texture2D = composed.ok_or_else(missing)?;
            device.CreateRenderTargetView(&composed, None, Some(&mut rtv))?;
            let iv = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            target.video_device.CreateVideoProcessorInputView(
                &composed,
                &enumerator,
                &iv,
                Some(&mut input),
            )?;
            let processor = target.video_device.CreateVideoProcessor(&enumerator, 0)?;
            let ov = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut outputs = Vec::with_capacity(POOL_SIZE);
            for t in target.pool {
                let mut v = None;
                target.video_device.CreateVideoProcessorOutputView(
                    t,
                    &enumerator,
                    &ov,
                    Some(&mut v),
                )?;
                outputs.push(v.ok_or_else(missing)?);
            }
            let ctx = target.video_context;
            let (bx, by, bw, bh) = letterbox(w, h, target.out_w, target.out_h);
            let src = RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            };
            let dst = RECT {
                left: bx as i32,
                top: by as i32,
                right: (bx + bw) as i32,
                bottom: (by + bh) as i32,
            };
            let full = RECT {
                left: 0,
                top: 0,
                right: target.out_w as i32,
                bottom: target.out_h as i32,
            };
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
            ctx.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            ctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            ctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            ctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            ctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
            ctx.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            ctx.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE::default(),
            );
            ctx.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: CS_OUTPUT_BT709_LIMITED,
                },
            );
            Ok(Blit {
                processor,
                composed,
                composed_rtv: rtv.ok_or_else(missing)?,
                input: input.ok_or_else(missing)?,
                outputs,
                width: w,
                height: h,
                format,
            })
        }
    }

    fn now_100ns(&self) -> i64 {
        let mut t = 0i64;
        // SAFETY: puntero de salida local.
        let _ = unsafe { QueryPerformanceCounter(&mut t) };
        qpc_to_100ns(t, self.qpc_freq)
    }

    fn update_pointer(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> Result<()> {
        if info.LastMouseUpdateTime != 0 {
            self.cursor.visible = info.PointerPosition.Visible.as_bool();
            self.cursor.pos = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );
        }
        if info.PointerShapeBufferSize != 0 {
            self.shape_buf
                .resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: el búfer tiene el tamaño indicado por DXGI para este fotograma.
            unsafe {
                dup.GetFramePointerShape(
                    self.shape_buf.len() as u32,
                    self.shape_buf.as_mut_ptr().cast(),
                    &mut needed,
                    &mut shape,
                )?
            };
            self.cursor.set_shape(
                &self.gpu.device,
                shape.Type,
                shape.Width,
                shape.Height,
                shape.Pitch,
                &self.shape_buf,
            )?;
        }
        Ok(())
    }

    // Devuelve None si no hay cambios (o la duplicación se está recuperando); en ese caso
    // el llamador puede reenviar `last_frame()` para mantener la cadencia mínima.
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
        if self.duplication.is_none() {
            match self.open_duplication() {
                Ok(()) => {
                    self.warned_lost = false;
                    tracing::info!("duplicación de pantalla restablecida");
                }
                Err(e) => {
                    if !self.warned_lost {
                        tracing::warn!("duplicación de pantalla no disponible, se reintenta: {e}");
                        self.warned_lost = true;
                    }
                    std::thread::sleep(timeout.min(Duration::from_millis(100)));
                    return Ok(None);
                }
            }
        }
        let Some(dup) = self.duplication.clone() else {
            return Ok(None);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let wait_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: punteros de salida locales; se llama a ReleaseFrame tras cada adquisición.
        let acquired = unsafe { dup.AcquireNextFrame(wait_ms, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                tracing::warn!(
                    "se perdió el acceso a la duplicación de pantalla (cambio de resolución, UAC o pantalla completa): se recrea"
                );
                self.duplication = None;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        let result = self.process_acquired(&dup, &info, resource);
        // SAFETY: empareja el AcquireNextFrame correcto de arriba.
        let _ = unsafe { dup.ReleaseFrame() };
        if result? {
            self.last_out = Instant::now();
            Ok(self.last_frame())
        } else {
            Ok(None)
        }
    }

    fn process_acquired(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<bool> {
        let mouse = info.LastMouseUpdateTime != 0;
        if info.AccumulatedFrames == 0 && !mouse {
            return Ok(false);
        }
        let Some(resource) = resource else {
            return Ok(false);
        };
        let _lock = MtGuard::new(&self.gpu.device);
        self.update_pointer(dup, info)?;
        let desktop: ID3D11Texture2D = resource.cast()?;
        let mut d = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: descripción de una textura devuelta por DXGI.
        unsafe { desktop.GetDesc(&mut d) };
        let stale = self
            .blit
            .as_ref()
            .is_none_or(|b| (b.width, b.height, b.format) != (d.Width, d.Height, d.Format));
        if stale {
            self.blit = Some(self.build_blit(d.Width, d.Height, d.Format)?);
            self.native = (d.Width, d.Height);
        }
        let Some(blit) = self.blit.as_ref() else {
            return Ok(false);
        };
        let slot = self.next_slot;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(blit.input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        // SAFETY: todas las copias y la composición ocurren en la GPU con objetos propios,
        // con el contexto bloqueado por `_lock`.
        let blt = unsafe {
            self.gpu.context.CopyResource(&blit.composed, &desktop);
            if self.draw_cursor {
                self.cursor.draw(
                    &self.gpu.context,
                    &blit.composed_rtv,
                    blit.width,
                    blit.height,
                );
            }
            let streams = [stream];
            let r = self.video_context.VideoProcessorBlt(
                &blit.processor,
                &blit.outputs[slot],
                0,
                &streams,
            );
            let [s] = streams;
            drop(ManuallyDrop::into_inner(s.pInputSurface));
            r
        };
        blt?;
        let mut pts = match (info.LastPresentTime, info.LastMouseUpdateTime) {
            (0, 0) => self.now_100ns(),
            (p, m) => qpc_to_100ns(p.max(m), self.qpc_freq),
        };
        if pts <= self.last_pts {
            pts = self.last_pts + 1;
        }
        self.last_pts = pts;
        self.last = Some((slot, pts));
        self.next_slot = (slot + 1) % POOL_SIZE;
        Ok(true)
    }

    pub fn last_frame(&self) -> Option<CapturedFrame<'_>> {
        let (slot, pts) = self.last?;
        Some(CapturedFrame {
            nv12: &self.pool[slot],
            width: self.out_w,
            height: self.out_h,
            pts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_wider_source_gets_bars_top_and_bottom() {
        assert_eq!(letterbox(2560, 1080, 1280, 720), (0, 90, 1280, 540));
    }

    #[test]
    fn letterbox_taller_source_gets_bars_on_sides() {
        let (x, y, w, h) = letterbox(1080, 1920, 1920, 1080);
        assert_eq!((y, h), (0, 1080));
        assert_eq!(w, 606);
        assert_eq!(x, 656);
    }

    #[test]
    fn letterbox_same_aspect_fills_output() {
        assert_eq!(letterbox(3840, 2160, 1280, 720), (0, 0, 1280, 720));
    }

    #[test]
    fn letterbox_dimensions_are_even() {
        let (x, y, w, h) = letterbox(1366, 768, 1280, 720);
        assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0);
        assert!(x + w <= 1280 && y + h <= 720);
    }

    #[test]
    fn qpc_conversion() {
        assert_eq!(qpc_to_100ns(10_000_000, 10_000_000), 10_000_000);
        assert_eq!(qpc_to_100ns(5, 10), 5_000_000);
    }
}
