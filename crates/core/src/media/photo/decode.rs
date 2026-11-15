// Still-picture decoding with WIC (JPEG, PNG, BMP, GIF, TIFF and HEIC when the Windows HEIF
// extension is installed). The result is premultiplied BGRA, upright (EXIF orientation applied),
// and no larger than `MAX_SIDE` pixels on its longest side so it fits any GPU.
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, Interface, w};

use crate::media::mf;
use crate::{Error, Result};

pub const MAX_SIDE: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    // Premultiplied BGRA, tightly packed.
    pub pixels: Vec<u8>,
}

// EXIF orientation (1..=8) of a JPEG file, 1 when there is none.
pub fn jpeg_orientation(data: &[u8]) -> u8 {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return 1;
    }
    let mut at = 2;
    while at + 4 <= data.len() && data[at] == 0xFF {
        let marker = data[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        // Standalone markers have no length; start of scan means the headers are over.
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            at += 2;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
        if len < 2 || at + 2 + len > data.len() {
            break;
        }
        let segment = &data[at + 4..at + 2 + len];
        if marker == 0xE1 && segment.starts_with(b"Exif\0\0") {
            return tiff_orientation(&segment[6..]).unwrap_or(1);
        }
        at += 2 + len;
    }
    1
}

fn tiff_orientation(tiff: &[u8]) -> Option<u8> {
    let big = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if big { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = usize::from(u16_at(ifd)?);
    for i in 0..count {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry)? == 0x0112 {
            let value = u16_at(entry + 8)?;
            return u8::try_from(value).ok().filter(|v| (1..=8).contains(v));
        }
    }
    None
}

// The WIC flip/rotate that makes a picture with this EXIF orientation upright.
pub fn orientation_transform(orientation: u8) -> WICBitmapTransformOptions {
    match orientation {
        2 => WICBitmapTransformFlipHorizontal,
        3 => WICBitmapTransformRotate180,
        4 => WICBitmapTransformFlipVertical,
        5 => WICBitmapTransformOptions(WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0),
        6 => WICBitmapTransformRotate90,
        7 => WICBitmapTransformOptions(WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0),
        8 => WICBitmapTransformRotate270,
        _ => WICBitmapTransformRotate0,
    }
}

// Size after shrinking the longest side to `MAX_SIDE`, keeping the aspect ratio.
pub fn capped_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_SIDE {
        return (width, height);
    }
    let scale = |v: u32| ((u64::from(v) * u64::from(MAX_SIDE) + u64::from(longest) / 2) / u64::from(longest)).max(1) as u32;
    (scale(width), scale(height))
}

pub fn decode(data: &[u8]) -> Result<Picture> {
    decode_with(data, None)
}

fn wic_error(what: &str, e: windows::core::Error) -> Error {
    Error::Media(format!("no se pudo {what} la imagen: {e}"))
}

fn orientation_from_metadata(frame: &IWICBitmapFrameDecode) -> Option<u8> {
    // SAFETY: consulta de metadatos de solo lectura sobre un fotograma propio.
    unsafe {
        let reader = frame.GetMetadataQueryReader().ok()?;
        let mut value = PROPVARIANT::default();
        reader.GetMetadataByName(w!("/ifd/{ushort=274}"), &mut value).ok()?;
        let orientation = u16::try_from(&value).ok()?;
        u8::try_from(orientation).ok().filter(|v| (1..=8).contains(v))
    }
}

// `forced` overrides the orientation found in the file (for tests).
fn decode_with(data: &[u8], forced: Option<u8>) -> Result<Picture> {
    if data.is_empty() {
        return Err(Error::Media("imagen vacía".into()));
    }
    mf::ensure_com()?;
    // SAFETY: objetos WIC propios creados y usados en este hilo; `data` vive durante la llamada.
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| wic_error("iniciar WIC para leer", e))?;
        let stream = factory.CreateStream().map_err(|e| wic_error("leer", e))?;
        stream.InitializeFromMemory(data).map_err(|e| wic_error("leer", e))?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null::<GUID>(), WICDecodeMetadataCacheOnDemand)
            .map_err(|e| wic_error("reconocer", e))?;
        let frame = decoder.GetFrame(0).map_err(|e| wic_error("abrir", e))?;
        let orientation = forced.unwrap_or_else(|| {
            if data.starts_with(&[0xFF, 0xD8]) {
                jpeg_orientation(data)
            } else {
                orientation_from_metadata(&frame).unwrap_or(1)
            }
        });
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).map_err(|e| wic_error("medir", e))?;
        if w == 0 || h == 0 {
            return Err(Error::Media("imagen sin tamaño".into()));
        }
        let mut source: IWICBitmapSource = frame.cast().map_err(|e| wic_error("leer", e))?;
        let (cw, ch) = capped_size(w, h);
        if (cw, ch) != (w, h) {
            let scaler = factory.CreateBitmapScaler().map_err(|e| wic_error("reducir", e))?;
            scaler
                .Initialize(&source, cw, ch, WICBitmapInterpolationModeFant)
                .map_err(|e| wic_error("reducir", e))?;
            source = scaler.cast().map_err(|e| wic_error("reducir", e))?;
        }
        let transform = orientation_transform(orientation);
        if transform != WICBitmapTransformRotate0 {
            let rotator = factory.CreateBitmapFlipRotator().map_err(|e| wic_error("girar", e))?;
            rotator.Initialize(&source, transform).map_err(|e| wic_error("girar", e))?;
            source = rotator.cast().map_err(|e| wic_error("girar", e))?;
        }
        let converter = factory.CreateFormatConverter().map_err(|e| wic_error("convertir", e))?;
        converter
            .Initialize(&source, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)
            .map_err(|e| wic_error("convertir", e))?;
        let (mut ow, mut oh) = (0u32, 0u32);
        converter.GetSize(&mut ow, &mut oh).map_err(|e| wic_error("medir", e))?;
        let stride = ow * 4;
        let mut pixels = vec![0u8; stride as usize * oh as usize];
        converter.CopyPixels(std::ptr::null(), stride, &mut pixels).map_err(|e| wic_error("copiar", e))?;
        Ok(Picture { width: ow, height: oh, pixels })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use windows::core::HSTRING;

    const GENERIC_WRITE: u32 = 0x4000_0000;

    // Encodes BGRA pixels with WIC (`container` is the PNG or JPEG container GUID).
    pub(crate) fn encode(container: &GUID, w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
        mf::ensure_com().unwrap();
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("rustplay-photo-{}-{n}.bin", std::process::id());
        let path = std::env::temp_dir().join(name);
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: objetos WIC propios en este hilo.
        unsafe {
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let stream = factory.CreateStream().unwrap();
            stream.InitializeFromFilename(&wide, GENERIC_WRITE).unwrap();
            let encoder = factory.CreateEncoder(container, std::ptr::null()).unwrap();
            encoder.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).unwrap();
            let frame = frame.unwrap();
            frame.Initialize(None).unwrap();
            frame.SetSize(w, h).unwrap();
            let mut format = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut format).unwrap();
            if format == GUID_WICPixelFormat32bppBGRA {
                frame.WritePixels(h, w * 4, bgra).unwrap();
            } else {
                // JPEG has no alpha: WIC asks for 24 bits per pixel.
                assert_eq!(format, GUID_WICPixelFormat24bppBGR);
                let rgb: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
                frame.WritePixels(h, w * 3, &rgb).unwrap();
            }
            frame.Commit().unwrap();
            encoder.Commit().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        bytes
    }

    pub(crate) fn png(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
        encode(&GUID_ContainerFormatPng, w, h, bgra)
    }

    #[test]
    fn decodes_a_png() {
        let pixels = [10, 20, 30, 255, 40, 50, 60, 255];
        let picture = decode(&png(2, 1, &pixels)).unwrap();
        assert_eq!((picture.width, picture.height), (2, 1));
        assert_eq!(picture.pixels, pixels);
    }

    #[test]
    fn decodes_a_jpeg() {
        let (w, h) = (32u32, 16u32);
        let mut pixels = Vec::new();
        for _ in 0..w * h {
            pixels.extend_from_slice(&[200, 40, 40, 255]);
        }
        let jpeg = encode(&GUID_ContainerFormatJpeg, w, h, &pixels);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let picture = decode(&jpeg).unwrap();
        assert_eq!((picture.width, picture.height), (w, h));
        // Lossy, but a flat colour stays close.
        assert!(picture.pixels[0].abs_diff(200) < 8 && picture.pixels[1].abs_diff(40) < 8 && picture.pixels[3] == 255);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not a picture at all, just text").is_err());
        assert!(decode(&[]).is_err());
    }

    // 2x1 picture [A B] decoded with a forced orientation.
    fn tagged(orientation: u8) -> Picture {
        let data = png(2, 1, &[1, 0, 0, 255, 2, 0, 0, 255]);
        decode_with(&data, Some(orientation)).unwrap()
    }

    fn blues(p: &Picture) -> Vec<u8> {
        p.pixels.chunks_exact(4).map(|px| px[0]).collect()
    }

    #[test]
    fn applies_exif_orientation() {
        assert_eq!(blues(&tagged(1)), vec![1, 2]);
        assert_eq!(blues(&tagged(2)), vec![2, 1]);
        assert_eq!(blues(&tagged(3)), vec![2, 1]);
        let rotated = tagged(6);
        assert_eq!((rotated.width, rotated.height), (1, 2));
        // 90 degrees clockwise: the left pixel ends on top.
        assert_eq!(blues(&rotated), vec![1, 2]);
        assert_eq!(blues(&tagged(8)), vec![2, 1]);
        // Transpose keeps the top-left pixel, transverse swaps the corners.
        assert_eq!(blues(&tagged(5)), vec![1, 2]);
        assert_eq!(blues(&tagged(7)), vec![2, 1]);
    }

    #[test]
    fn reads_the_orientation_from_exif() {
        // SOI, APP1 "Exif" with a little-endian TIFF holding one entry: Orientation = 6.
        let mut tiff = b"II*\x00\x08\x00\x00\x00".to_vec();
        tiff.extend_from_slice(&[1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        let len = (2 + 6 + tiff.len()) as u16;
        jpeg.extend_from_slice(&len.to_be_bytes());
        jpeg.extend_from_slice(b"Exif\x00\x00");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(jpeg_orientation(&jpeg), 6);
        assert_eq!(jpeg_orientation(&[0xFF, 0xD8, 0xFF, 0xD9]), 1);
        assert_eq!(jpeg_orientation(b"nope"), 1);
    }

    #[test]
    fn caps_big_pictures() {
        assert_eq!(capped_size(1000, 500), (1000, 500));
        assert_eq!(capped_size(8192, 4096), (4096, 2048));
        assert_eq!(capped_size(100, 12000), (34, 4096));
    }
}
