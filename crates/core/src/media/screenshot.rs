// Still images of the mirror window: read a BGRA texture back from the GPU and save it as PNG
// with WIC.
use std::path::Path;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::HSTRING;

use super::capture::MtGuard;
use super::gpu::GpuDevice;
use super::mf;
use crate::{Error, Result};

const GENERIC_WRITE: u32 = 0x4000_0000;

// Tightly packed 32-bit BGRA pixels with opaque alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

// Copies a BGRA texture through a staging texture into system memory.
pub fn read_texture(gpu: &GpuDevice, texture: &ID3D11Texture2D) -> Result<Image> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: descripción de una textura válida del llamador.
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(Error::Media(format!("formato de textura no admitido para la captura: {}", desc.Format.0)));
    }
    let (w, h) = (desc.Width, desc.Height);
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut staging = None;
    // SAFETY: descripción válida y puntero de salida local.
    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging = staging.ok_or_else(|| Error::Media("no se pudo crear la textura de lectura".into()))?;
    let row = w as usize * 4;
    let mut pixels = vec![0u8; row * h as usize];
    let _lock = MtGuard::new(&gpu.device);
    // SAFETY: la textura de lectura se mapea solo mientras se copian las filas.
    unsafe {
        gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, 0, None);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let src = mapped.pData as *const u8;
        for y in 0..h as usize {
            std::ptr::copy_nonoverlapping(src.add(y * mapped.RowPitch as usize), pixels.as_mut_ptr().add(y * row), row);
        }
        gpu.context.Unmap(&staging, 0);
    }
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    Ok(Image { width: w, height: h, pixels })
}

// Writes `image` to `path` as a PNG file (replacing it).
pub fn save_png(path: &Path, image: &Image) -> Result<()> {
    let stride = image.width * 4;
    if image.width == 0 || image.height == 0 || image.pixels.len() != (stride * image.height) as usize {
        return Err(Error::Media("imagen vacía o con un tamaño incoherente".into()));
    }
    mf::ensure_com()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: objetos WIC propios creados y usados en este hilo.
    let written = unsafe {
        (|| -> Result<()> {
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = factory.CreateStream()?;
            stream.InitializeFromFilename(&wide, GENERIC_WRITE)?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(|| Error::Media("WIC no creó el fotograma".into()))?;
            frame.Initialize(None)?;
            frame.SetSize(image.width, image.height)?;
            let mut format = GUID_WICPixelFormat32bppBGRA;
            frame.SetPixelFormat(&mut format)?;
            if format != GUID_WICPixelFormat32bppBGRA {
                return Err(Error::Media("el codificador PNG no acepta BGRA de 32 bits".into()));
            }
            frame.WritePixels(image.height, stride, &image.pixels)?;
            frame.Commit()?;
            encoder.Commit()?;
            Ok(())
        })()
    };
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: u32, h: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255]);
            }
        }
        pixels
    }

    fn check_png(path: &Path, w: u32, h: u32) {
        let bytes = std::fs::read(path).expect("archivo PNG");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), w);
        assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), h);
        assert!(bytes.len() > 100);
    }

    #[test]
    fn saves_a_png() {
        let dir = std::env::temp_dir().join(format!("rustplay-png-{}", std::process::id()));
        let path = dir.join("captura.png");
        let image = Image { width: 64, height: 48, pixels: pattern(64, 48) };
        save_png(&path, &image).expect("PNG");
        check_png(&path, 64, 48);
        assert!(save_png(&path, &Image { width: 2, height: 2, pixels: vec![0; 3] }).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_a_texture_back_and_saves_it() {
        let Ok(gpu) = GpuDevice::create() else {
            eprintln!("sin GPU: se omite la prueba de captura");
            return;
        };
        let (w, h) = (96u32, 40u32);
        let mut pixels = pattern(w, h);
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 7;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
        let mut tex = None;
        // SAFETY: datos iniciales válidos durante la llamada.
        unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).expect("textura") };
        let image = read_texture(&gpu, &tex.unwrap()).expect("lectura");
        assert_eq!((image.width, image.height), (w, h));
        assert_eq!(&image.pixels[..3], &pixels[..3]);
        assert!(image.pixels.iter().skip(3).step_by(4).all(|&a| a == 255), "alfa opaco");
        let dir = std::env::temp_dir().join(format!("rustplay-shot-{}", std::process::id()));
        let path = dir.join("pantalla.png");
        save_png(&path, &image).expect("PNG");
        check_png(&path, w, h);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
