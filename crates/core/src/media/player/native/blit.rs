// Converts a decoded NV12 picture to BGRA and scales it into the player's frame texture, on the
// GPU, with the D3D11 video processor.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;
use windows::core::Interface;

use crate::media::gpu::GpuDevice;
use crate::media::video::DecodedFrame;
use crate::{Error, Result};

const VIEW_CACHE: usize = 24;

pub(super) struct Blitter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    // (enumerator, processor) for (input width, input height, output width, output height).
    vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, [u32; 4])>,
    // Input views by decoder texture and slice; the decoder reuses a small pool of textures.
    views: Vec<(usize, u32, ID3D11VideoProcessorInputView)>,
    out: Option<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

impl Blitter {
    pub fn new(gpu: &Arc<GpuDevice>) -> Result<Self> {
        Ok(Self { vdev: gpu.device.cast()?, vctx: gpu.context.cast()?, vp: None, views: Vec::new(), out: None })
    }

    fn ensure_processor(&mut self, input: (u32, u32), output: (u32, u32)) -> Result<()> {
        let key = [input.0, input.1, output.0, output.1];
        if self.vp.as_ref().is_some_and(|(_, _, k)| *k == key) {
            return Ok(());
        }
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        // SAFETY: descripción válida; el contexto de vídeo es del mismo dispositivo.
        unsafe {
            let en = self.vdev.CreateVideoProcessorEnumerator(&desc)?;
            let vp = self.vdev.CreateVideoProcessor(&en, 0)?;
            self.vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            self.vctx.VideoProcessorSetStreamOutputRate(&vp, 0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, true, None);
            // YCbCr BT.709, limited range in; full range RGB out.
            let input_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            self.vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);
            self.vp = Some((en, vp, key));
        }
        self.views.clear();
        self.out = None;
        Ok(())
    }

    fn input_view(&mut self, frame: &DecodedFrame) -> Result<ID3D11VideoProcessorInputView> {
        let key = Interface::as_raw(&frame.texture) as usize;
        if let Some((_, _, view)) = self.views.iter().find(|(t, s, _)| *t == key && *s == frame.subresource) {
            return Ok(view.clone());
        }
        let en = self.vp.as_ref().map(|(e, _, _)| e.clone()).ok_or_else(|| Error::Media("sin procesador de vídeo".into()))?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: frame.subresource },
            },
        };
        let mut view = None;
        // SAFETY: textura del decodificador y enumerador del mismo dispositivo.
        unsafe { self.vdev.CreateVideoProcessorInputView(&frame.texture, &en, &desc, Some(&mut view))? };
        let view = view.ok_or_else(|| Error::Media("no se pudo crear la vista de entrada".into()))?;
        if self.views.len() >= VIEW_CACHE {
            self.views.remove(0);
        }
        self.views.push((key, frame.subresource, view.clone()));
        Ok(view)
    }

    fn output_view(&mut self, target: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        if let Some((tex, view)) = &self.out
            && tex == target
        {
            return Ok(view.clone());
        }
        let en = self.vp.as_ref().map(|(e, _, _)| e.clone()).ok_or_else(|| Error::Media("sin procesador de vídeo".into()))?;
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        // SAFETY: textura de destino con BIND_RENDER_TARGET y enumerador del mismo dispositivo.
        unsafe { self.vdev.CreateVideoProcessorOutputView(target, &en, &desc, Some(&mut view))? };
        let view = view.ok_or_else(|| Error::Media("no se pudo crear la vista de salida".into()))?;
        self.out = Some((target.clone(), view.clone()));
        Ok(view)
    }

    // Draws `frame` inside `dst` (x, y, width, height) of `target`, which is `size` pixels big.
    pub fn blit(&mut self, frame: &DecodedFrame, target: &ID3D11Texture2D, size: (u32, u32), dst: (f32, f32, f32, f32)) -> Result<()> {
        self.ensure_processor((frame.width, frame.height), size)?;
        let view = self.input_view(frame)?;
        let out = self.output_view(target)?;
        let Some((_, vp, _)) = &self.vp else { return Ok(()) };
        let source = RECT { left: 0, top: 0, right: frame.width as i32, bottom: frame.height as i32 };
        let dest = RECT {
            left: dst.0.round() as i32,
            top: dst.1.round() as i32,
            right: (dst.0 + dst.2).round() as i32,
            bottom: (dst.1 + dst.3).round() as i32,
        };
        let full = RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 };
        // SAFETY: vistas y procesador creados sobre el mismo dispositivo; el flujo se libera tras el Blt.
        unsafe {
            self.vctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&source));
            self.vctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dest));
            self.vctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&full));
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(view)),
                ..Default::default()
            };
            let blt = self.vctx.VideoProcessorBlt(vp, &out, 0, std::slice::from_ref(&stream));
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt?;
        }
        Ok(())
    }
}
