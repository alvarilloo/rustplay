// Re-encodes decoded pictures to H.264 so a recording can start at any moment. The iPhone sends a
// keyframe (IDR) only when the stream starts or changes, so a recording that has to begin on the
// device's own keyframe would replay everything since then. The GPU encoder makes a keyframe of
// the current picture instead, and the rest follows from it.

use std::sync::Arc;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use super::gpu::GpuDevice;
use super::video::{DecodedFrame, EncoderConfig, H264Encoder};
use crate::{Error, Result};

pub struct Reencoder {
    gpu: Arc<GpuDevice>,
    encoder: H264Encoder,
    size: (u32, u32),
    // Standalone NV12 copies of the decoder's pictures (it hands out slices of a texture array). A
    // few of them rotate, since the encoder may still be reading the previous one.
    copies: Vec<ID3D11Texture2D>,
    next: usize,
}

const COPIES: usize = 4;

// SAFETY: used from a single thread at a time (the video thread); the device is multithread protected.
unsafe impl Send for Reencoder {}

impl Reencoder {
    pub fn new(gpu: &Arc<GpuDevice>, width: u32, height: u32) -> Result<Self> {
        let (width, height) = (width & !1, height & !1);
        if width == 0 || height == 0 {
            return Err(Error::Media("imagen demasiado pequeña para grabar".into()));
        }
        // About 0.1 bit per pixel per frame at 30 fps, between 3 and 14 Mbit/s.
        let bitrate = (u64::from(width) * u64::from(height) * 3).clamp(3_000_000, 14_000_000) as u32;
        let mut encoder = H264Encoder::new(Arc::clone(gpu), EncoderConfig { width, height, fps: 30, bitrate })?;
        encoder.request_keyframe();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let mut copies = Vec::with_capacity(COPIES);
        for _ in 0..COPIES {
            let mut copy = None;
            // SAFETY: descripción válida.
            unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut copy))? };
            copies.push(copy.ok_or_else(|| Error::Media("sin textura para recodificar".into()))?);
        }
        Ok(Self { gpu: Arc::clone(gpu), encoder, size: (width, height), copies, next: 0 })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    // Encodes `frame` and hands every finished access unit (data, pts) to `sink`.
    pub fn encode(&mut self, frame: &DecodedFrame, sink: &mut dyn FnMut(&[u8], i64)) -> Result<()> {
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: self.size.0, bottom: self.size.1, back: 1 };
        let copy = self.copies[self.next % COPIES].clone();
        self.next += 1;
        // SAFETY: ambas texturas son NV12 del mismo dispositivo y la región cabe en las dos.
        unsafe {
            self.gpu.context.CopySubresourceRegion(&copy, 0, 0, 0, 0, &frame.texture, frame.subresource, Some(&region));
        }
        if let Some(out) = self.encoder.encode(&copy, frame.pts)? {
            sink(out.data, out.pts);
        }
        while let Some(out) = self.encoder.poll_output()? {
            sink(out.data, out.pts);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::recorder::is_keyframe;
    use crate::media::video::H264Decoder;
    use windows::Win32::Graphics::Direct3D11::{D3D11_SUBRESOURCE_DATA, ID3D11Texture2D};

    fn nv12(gpu: &GpuDevice, w: u32, h: u32, frame: u32) -> Option<ID3D11Texture2D> {
        let mut pixels = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                pixels[(y * w + x) as usize] = ((x + y + frame * 8) % 220 + 16) as u8;
            }
        }
        for c in &mut pixels[(w * h) as usize..] {
            *c = 128;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w, SysMemSlicePitch: 0 };
        let mut tex = None;
        // SAFETY: datos iniciales válidos durante la llamada.
        unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).ok()? };
        tex
    }

    // A stream whose own keyframe is long gone still yields a recording that starts on a
    // keyframe made from the picture at hand.
    #[test]
    #[ignore = "needs a GPU with an H.264 encoder and decoder"]
    fn starts_on_a_fresh_keyframe_from_decoded_pictures() {
        crate::media::init_thread().expect("COM");
        let gpu = GpuDevice::create().expect("GPU");
        let (w, h) = (320u32, 240u32);
        let mut source = H264Encoder::new(Arc::clone(&gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 }).expect("codificador");
        let textures: Vec<_> = (0..4).filter_map(|i| nv12(&gpu, w, h, i)).collect();
        let mut decoder = H264Decoder::new(Arc::clone(&gpu)).expect("decodificador");
        let mut reencoder = Reencoder::new(&gpu, w, h).expect("recodificador");
        let mut units: Vec<(Vec<u8>, i64)> = Vec::new();
        let mut decoded = 0;
        for i in 0..40u32 {
            let pts = i64::from(i) * 10_000_000 / 30;
            let mut stream = Vec::new();
            if let Some(f) = source.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                stream.push((f.data.to_vec(), f.pts));
            }
            while let Some(f) = source.poll_output().expect("salida") {
                stream.push((f.data.to_vec(), f.pts));
            }
            for (data, pts) in stream {
                // Recording only begins at frame 20, long after the source's keyframe.
                if let Some(frame) = decoder.decode(&data, pts).expect("decodificar") {
                    decoded += 1;
                    if decoded > 20 {
                        reencoder.encode(&frame, &mut |d, p| units.push((d.to_vec(), p))).expect("recodificar");
                    }
                }
            }
        }
        assert!(units.len() >= 10, "pocas unidades: {}", units.len());
        assert!(is_keyframe(&units[0].0), "la grabación no empieza con un fotograma clave");
        assert!(units.windows(2).all(|w| w[1].1 >= w[0].1), "tiempos desordenados");
    }
}
