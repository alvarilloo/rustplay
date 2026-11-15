use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{GUID, Interface};

use super::DecodedFrame;
use crate::media::gpu::GpuDevice;
use crate::media::mf;
use crate::{Error, Result};

const CLSID_H264_DECODER: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);
const INITIAL_INPUT_CAPACITY: u32 = 256 * 1024;

pub struct H264Decoder {
    pub gpu: Arc<GpuDevice>,
    xf: IMFTransform,
    in_sample: IMFSample,
    in_buf: IMFMediaBuffer,
    in_cap: u32,
    out_sample: Option<(IMFSample, IMFMediaBuffer)>,
    hardware: bool,
    width: u32,
    height: u32,
    pitch: u32,
    upload: Option<ID3D11Texture2D>,
}

// SAFETY: la MFT vive en el apartamento MTA y el dispositivo D3D11 está protegido
// para multihilo; `&mut self` garantiza uso exclusivo desde un solo hilo a la vez.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new(gpu: Arc<GpuDevice>) -> Result<Self> {
        let xf = mf::create_transform(&CLSID_H264_DECODER)?;
        // SAFETY: llamadas COM sobre la MFT recién creada y el gestor DXGI del dispositivo.
        let hardware = unsafe {
            let attrs = xf.GetAttributes()?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1;
            let ok = aware
                && xf
                    .ProcessMessage(
                        MFT_MESSAGE_SET_D3D_MANAGER,
                        Interface::as_raw(&gpu.manager) as usize,
                    )
                    .is_ok();
            if !ok {
                tracing::warn!(
                    "el decodificador H.264 no admite D3D11: se usa decodificación por software y subida a la GPU"
                );
            }
            ok
        };
        // SAFETY: tipos de medio propios; se negocia entrada H.264 y salida NV12.
        unsafe {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            xf.SetInputType(0, &t, 0)?;
        }
        // SAFETY: búfer de entrada creado una sola vez y reutilizado.
        let (in_buf, in_sample) = unsafe {
            let in_buf = MFCreateMemoryBuffer(INITIAL_INPUT_CAPACITY)?;
            let in_sample = MFCreateSample()?;
            in_sample.AddBuffer(&in_buf)?;
            (in_buf, in_sample)
        };
        let mut dec = Self {
            gpu,
            xf,
            in_sample,
            in_buf,
            in_cap: INITIAL_INPUT_CAPACITY,
            out_sample: None,
            hardware,
            width: 0,
            height: 0,
            pitch: 0,
            upload: None,
        };
        dec.negotiate_output()?;
        // SAFETY: mensajes de streaming sobre una MFT propia.
        unsafe {
            dec.xf.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            dec.xf
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.xf
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(dec)
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn negotiate_output(&mut self) -> Result<()> {
        // SAFETY: enumeración y selección del tipo de salida NV12 de la MFT.
        unsafe {
            let mut i = 0;
            let chosen = loop {
                let Ok(t) = self.xf.GetOutputAvailableType(0, i) else {
                    return Err(Error::Media(
                        "el decodificador H.264 no ofrece salida NV12".into(),
                    ));
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    break t;
                }
                i += 1;
            };
            self.xf.SetOutputType(0, &chosen, 0)?;
            let cur = self.xf.GetOutputCurrentType(0)?;
            if let Ok(sz) = cur.GetUINT64(&MF_MT_FRAME_SIZE) {
                self.width = (sz >> 32) as u32;
                self.height = sz as u32;
            }
            // El tamaño codificado va alineado a 16; se usa el área visible del SPS.
            for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
                let mut area = MFVideoArea::default();
                let buf = std::slice::from_raw_parts_mut(
                    &mut area as *mut MFVideoArea as *mut u8,
                    size_of::<MFVideoArea>(),
                );
                if cur.GetBlob(key, buf, None).is_ok() && area.Area.cx > 0 && area.Area.cy > 0 {
                    self.width = area.Area.cx as u32;
                    self.height = area.Area.cy as u32;
                    break;
                }
            }
            self.pitch = cur
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| (s as i32).unsigned_abs())
                .unwrap_or(self.width);
            let info = self.xf.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            self.out_sample = if provides {
                None
            } else {
                let buf = MFCreateMemoryBuffer(info.cbSize.max(self.width * self.height * 3 / 2))?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                Some((s, buf))
            };
        }
        Ok(())
    }

    // Entrada en Annex B; SPS/PPS pueden ir delante de un IDR. Si una unidad produce
    // varias imágenes se devuelve solo la última (mínima latencia).
    pub fn decode(&mut self, access_unit: &[u8], pts: i64) -> Result<Option<DecodedFrame>> {
        if access_unit.is_empty() {
            return Ok(None);
        }
        let len = access_unit.len() as u32;
        // SAFETY: el búfer se bloquea durante la copia y se amplía antes si hace falta.
        unsafe {
            if len > self.in_cap {
                let cap = len.next_power_of_two();
                let buf = MFCreateMemoryBuffer(cap)?;
                self.in_sample.RemoveAllBuffers()?;
                self.in_sample.AddBuffer(&buf)?;
                self.in_buf = buf;
                self.in_cap = cap;
            }
            let mut p = std::ptr::null_mut();
            self.in_buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(access_unit.as_ptr(), p, access_unit.len());
            self.in_buf.Unlock()?;
            self.in_buf.SetCurrentLength(len)?;
            self.in_sample.SetSampleTime(pts)?;
        }
        let mut earlier = None;
        // SAFETY: la muestra de entrada es propia y se reutiliza.
        let first = unsafe { self.xf.ProcessInput(0, &self.in_sample, 0) };
        if let Err(e) = first {
            if e.code() != MF_E_NOTACCEPTING {
                return Err(e.into());
            }
            earlier = self.drain()?;
            // SAFETY: igual que arriba.
            unsafe { self.xf.ProcessInput(0, &self.in_sample, 0)? };
        }
        let newer = self.drain()?;
        Ok(newer.or(earlier))
    }

    fn drain(&mut self) -> Result<Option<DecodedFrame>> {
        let mut latest = None;
        loop {
            if let Some((_, b)) = &self.out_sample {
                // SAFETY: búfer propio.
                unsafe { b.SetCurrentLength(0)? };
            }
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.out_sample.as_ref().map(|(s, _)| s.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: `out` vive durante la llamada; las referencias se liberan abajo.
            let res = unsafe { self.xf.ProcessOutput(0, &mut out, &mut status) };
            let [o] = out;
            let sample = ManuallyDrop::into_inner(o.pSample);
            drop(ManuallyDrop::into_inner(o.pEvents));
            match res {
                Ok(()) => {
                    if let Some(s) = sample {
                        latest = Some(self.frame_from(s)?);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(latest),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate_output()?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn frame_from(&mut self, sample: IMFSample) -> Result<DecodedFrame> {
        // SAFETY: la muestra procede de la MFT; el recurso D3D11 se obtiene sin copia.
        unsafe {
            let pts = sample.GetSampleTime().unwrap_or(0);
            let buf = sample.GetBufferByIndex(0)?;
            if let Ok(dxgi) = buf.cast::<IMFDXGIBuffer>() {
                let mut raw: *mut c_void = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                if !raw.is_null() {
                    let texture = ID3D11Texture2D::from_raw(raw);
                    let subresource = dxgi.GetSubresourceIndex()?;
                    return Ok(DecodedFrame {
                        texture,
                        subresource,
                        width: self.width,
                        height: self.height,
                        pts,
                        _sample: Some(sample),
                    });
                }
            }
            let texture = self.upload_software(&buf)?;
            Ok(DecodedFrame {
                texture,
                subresource: 0,
                width: self.width,
                height: self.height,
                pts,
                _sample: None,
            })
        }
    }

    // Ruta de respaldo sin DXVA: una única copia CPU a GPU del NV12 decodificado por software.
    unsafe fn upload_software(&mut self, buf: &IMFMediaBuffer) -> Result<ID3D11Texture2D> {
        let tex_h = self.height.div_ceil(16) * 16;
        let stale = match &self.upload {
            Some(t) => {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: descripción de una textura propia.
                unsafe { t.GetDesc(&mut d) };
                d.Width != self.width || d.Height != tex_h
            }
            None => true,
        };
        if stale {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: tex_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let mut tex = None;
            // SAFETY: descripción válida y puntero de salida local.
            unsafe {
                self.gpu
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut tex))?
            };
            self.upload = tex;
        }
        let tex = self
            .upload
            .clone()
            .ok_or_else(|| Error::Media("no se pudo crear la textura de subida".into()))?;
        let mut p = std::ptr::null_mut();
        let mut pitch = self.pitch as i32;
        // SAFETY: el búfer se bloquea mientras se copia a la GPU.
        unsafe {
            let b2d = buf.cast::<IMF2DBuffer>().ok();
            match &b2d {
                Some(b) => b.Lock2D(&mut p, &mut pitch)?,
                None => buf.Lock(&mut p, None, None)?,
            }
            self.gpu
                .context
                .UpdateSubresource(&tex, 0, None, p as *const c_void, pitch as u32, 0);
            match &b2d {
                Some(b) => b.Unlock2D()?,
                None => buf.Unlock()?,
            }
        }
        Ok(tex)
    }
}
