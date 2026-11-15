use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::IMFSample;

#[cfg(test)]
use super::gpu::GpuDevice;
#[cfg(test)]
use crate::Result;

mod cinema;
mod decoder;
mod encoder;
mod overlay;
mod presenter;

pub use decoder::H264Decoder;
pub use encoder::*;
pub use presenter::{OverlayTexts, Presenter, PresenterOptions, Scaling};

pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub subresource: u32,
    pub width: u32,
    pub height: u32,
    pub pts: i64,
    _sample: Option<IMFSample>,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use windows::Win32::Media::MediaFoundation::*;
    use windows::core::GUID;

    use super::*;
    use crate::media::mf;

    const CLSID_H264_ENCODER: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);
    const W: u32 = 640;
    const H: u32 = 360;

    fn pack(a: u32, b: u32) -> u64 {
        ((a as u64) << 32) | b as u64
    }

    fn encode_stream_sized(frames: u32, w: u32, h: u32) -> Result<Vec<Vec<u8>>> {
        let xf = mf::create_transform(&CLSID_H264_ENCODER)?;
        // SAFETY: tipos de medio propios sobre la MFT recién creada.
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, 2_000_000)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(30, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, 66)?;
            xf.SetOutputType(0, &out, 0)?;
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(30, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            xf.SetInputType(0, &inp, 0)?;
        }
        let mut units = Vec::new();
        // SAFETY: muestras y búferes propios; la MFT codificadora se maneja de forma síncrona.
        unsafe {
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let info = xf.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let out_buf = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?;
            let out_sample = MFCreateSample()?;
            out_sample.AddBuffer(&out_buf)?;
            let mut raw = vec![0u8; (w * h * 3 / 2) as usize];
            let drain = |xf: &IMFTransform, units: &mut Vec<Vec<u8>>| -> Result<()> {
                loop {
                    out_buf.SetCurrentLength(0)?;
                    let mut o = [MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: 0,
                        pSample: std::mem::ManuallyDrop::new(
                            (!provides).then(|| out_sample.clone()),
                        ),
                        dwStatus: 0,
                        pEvents: std::mem::ManuallyDrop::new(None),
                    }];
                    let mut st = 0u32;
                    let r = xf.ProcessOutput(0, &mut o, &mut st);
                    let [o] = o;
                    let s = std::mem::ManuallyDrop::into_inner(o.pSample);
                    drop(std::mem::ManuallyDrop::into_inner(o.pEvents));
                    match r {
                        Ok(()) => {
                            if let Some(s) = s {
                                let b = s.ConvertToContiguousBuffer()?;
                                let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
                                b.Lock(&mut p, None, Some(&mut len))?;
                                units.push(std::slice::from_raw_parts(p, len as usize).to_vec());
                                b.Unlock()?;
                            }
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                        Err(e) => return Err(e.into()),
                    }
                }
            };
            for i in 0..frames {
                for y in 0..h {
                    for x in 0..w {
                        raw[(y * w + x) as usize] = ((x + i * 4) / 3 % 200 + 20) as u8;
                    }
                }
                for (k, c) in raw[(w * h) as usize..].iter_mut().enumerate() {
                    let (col, row) = ((k as u32 % w) / 2, k as u32 / w);
                    *c = if k % 2 == 0 { (60 + col * 140 / (w / 2) + i * 2) as u8 } else { (80 + row * 150 / (h / 2)) as u8 };
                }
                let buf = MFCreateMemoryBuffer(raw.len() as u32)?;
                let mut p = std::ptr::null_mut();
                buf.Lock(&mut p, None, None)?;
                std::ptr::copy_nonoverlapping(raw.as_ptr(), p, raw.len());
                buf.Unlock()?;
                buf.SetCurrentLength(raw.len() as u32)?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                s.SetSampleTime(i as i64 * 333_333)?;
                s.SetSampleDuration(333_333)?;
                xf.ProcessInput(0, &s, 0)?;
                drain(&xf, &mut units)?;
            }
            xf.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            drain(&xf, &mut units)?;
        }
        Ok(units)
    }

    fn encode_stream(frames: u32) -> Result<Vec<Vec<u8>>> {
        encode_stream_sized(frames, W, H)
    }

    #[test]
    #[ignore = "requiere GPU, ventana y escritorio interactivo"]
    fn decode_and_present_synthetic_stream() {
        let units = encode_stream(60).expect("codificación sintética");
        assert!(units.len() >= 30, "pocas unidades: {}", units.len());
        assert!(units[0].starts_with(&[0, 0, 0, 1]));
        let gpu = GpuDevice::create().expect("dispositivo GPU");
        let mut dec = H264Decoder::new(gpu.clone()).expect("decodificador");
        let mut pres = Presenter::new(gpu, "Prueba de espejo", PresenterOptions::default()).expect("ventana");
        let start = Instant::now();
        let mut shown = 0;
        for (i, au) in units.iter().enumerate() {
            if let Some(f) = dec.decode(au, i as i64 * 333_333).expect("decodificar") {
                assert_eq!((f.width, f.height), (W, H));
                pres.present(&f).expect("presentar");
                shown += 1;
                std::thread::sleep(Duration::from_millis(16));
            }
        }
        eprintln!(
            "hardware={} imágenes={} en {:?}",
            dec.is_hardware(),
            shown,
            start.elapsed()
        );
        assert!(shown >= units.len() - 2);
    }

    // Renders a portrait picture in a landscape window and saves what the window shows (PNG in
    // $RUSTPLAY_TEST_OUT or the temp folder) for inspection.
    #[test]
    #[ignore = "requiere GPU, ventana y escritorio interactivo"]
    fn blurred_background_and_cinema_mode_look() {
        use crate::media::screenshot::save_png;
        let out = std::env::var_os("RUSTPLAY_TEST_OUT").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let units = encode_stream_sized(24, 360, 640).expect("codificación sintética");
        let gpu = GpuDevice::create().expect("dispositivo GPU");
        let mut dec = H264Decoder::new(gpu.clone()).expect("decodificador");
        let options = PresenterOptions {
            blur_background: true,
            stats: true,
            stats_text: "{width}×{height} · {fps} fps".into(),
            overlay: OverlayTexts {
                toast_blur: "Fondo difuminado".into(),
                toast_cinema: "Modo cine".into(),
                toast_on: "activado".into(),
                toast_off: "desactivado".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pres = Presenter::new(gpu, "Prueba de cine", options).expect("ventana");
        let mut frames = units.iter().enumerate().filter_map(|(i, au)| dec.decode(au, i as i64 * 333_333).expect("decodificar"));
        let first = frames.next().expect("primera imagen");
        pres.present(&first).expect("presentar");
        // A landscape window, as a user would drag it.
        std::thread::sleep(Duration::from_millis(300));
        pres.test_resize(1000, 600);
        std::thread::sleep(Duration::from_millis(300));
        let (cw, ch) = pres.test_client_size();
        assert!(cw > ch, "ventana apaisada: {cw}×{ch}");
        let mut shown = |pres: &mut Presenter, n: usize| {
            for _ in 0..n {
                let Some(f) = frames.next() else { return };
                pres.present(&f).expect("presentar");
                std::thread::sleep(Duration::from_millis(16));
            }
        };
        shown(&mut pres, 6);
        let grab = |pres: &mut Presenter, name: &str, more: &mut dyn FnMut(&mut Presenter)| {
            pres.grab_next_window();
            more(pres);
            let image = pres.take_window_capture().expect("captura de la ventana");
            save_png(&out.join(name), &image).expect("guardar PNG");
            image
        };
        let pixel = |image: &crate::media::screenshot::Image, x: u32, y: u32| {
            let i = ((y * image.width + x) * 4) as usize;
            (image.pixels[i + 2], image.pixels[i + 1], image.pixels[i])
        };
        let blurred = grab(&mut pres, "cinema-blur.png", &mut |p| shown(p, 1));
        assert_eq!((blurred.width, blurred.height), (cw, ch));
        let bar = pixel(&blurred, 20, ch / 2);
        assert!(bar.0 as u32 + bar.1 as u32 + bar.2 as u32 > 30, "la barra izquierda debe tener color: {bar:?}");
        // B turns the blur off: the bars go black again.
        pres.test_key(0x42);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!pres.test_state().1);
        let plain = grab(&mut pres, "cinema-plain-with-toast.png", &mut |p| shown(p, 1));
        assert_eq!(pixel(&plain, 20, ch / 2), (0, 0, 0));
        pres.test_key(0x42);
        std::thread::sleep(Duration::from_millis(100));
        assert!(pres.test_state().1);
        // C: borderless, keeping the client size; L and T flip their flags.
        let before = pres.test_style();
        pres.test_key(0x43);
        std::thread::sleep(Duration::from_millis(300));
        let style = pres.test_style();
        assert_ne!(before & 0x00C0_0000, 0);
        assert_eq!(style & 0x00C0_0000, 0, "sin barra de título");
        assert!(pres.test_state().0);
        assert_eq!(pres.test_client_size(), (cw, ch), "la imagen no se mueve ni cambia de tamaño");
        pres.test_key(0x4C);
        pres.test_key(0x54);
        std::thread::sleep(Duration::from_millis(100));
        let (borderless, blur, locked, topmost) = pres.test_state();
        assert!(borderless && blur && locked && topmost);
        let cinema = grab(&mut pres, "cinema-borderless.png", &mut |p| shown(p, 1));
        assert_eq!((cinema.width, cinema.height), (cw, ch));
        pres.test_key(0x43);
        std::thread::sleep(Duration::from_millis(300));
        assert_ne!(pres.test_style() & 0x00C0_0000, 0, "vuelve la barra de título");
        assert_eq!(pres.test_client_size(), (cw, ch));
    }
}
