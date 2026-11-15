use std::sync::Arc;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIFactory1,
};
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, MFCreateDXGIDeviceManager};
use windows::core::Interface;

use super::mf;
use crate::{Error, Result};

pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub manager: IMFDXGIDeviceManager,
    pub reset_token: u32,
}

unsafe impl Send for GpuDevice {} // SAFETY: device created with video support and ID3D10Multithread protection
unsafe impl Sync for GpuDevice {}

const LEVELS: [D3D_FEATURE_LEVEL; 4] =
    [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0];
// The driver may refuse video devices for a moment (E_OUTOFMEMORY) while another program, such as
// a game recorder, is using the video engine; it recovers on its own.
const ATTEMPTS: u32 = 4;
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

fn try_create(adapter: Option<&IDXGIAdapter>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let kind: D3D_DRIVER_TYPE = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: punteros de salida válidos durante la llamada.
    unsafe {
        D3D11CreateDevice(
            adapter,
            kind,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    match (device, context) {
        (Some(device), Some(context)) => Ok((device, context)),
        _ => Err(Error::Media("no se pudo crear el dispositivo D3D11".into())),
    }
}

// Hardware adapters other than the default one (e.g. an integrated GPU next to a dedicated one).
fn other_adapters() -> Vec<IDXGIAdapter> {
    let mut adapters = Vec::new();
    // SAFETY: enumeración DXGI con objetos propios.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return adapters };
        let mut i = 1;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let software = adapter.GetDesc1().is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0);
            if !software && let Ok(adapter) = adapter.cast::<IDXGIAdapter>() {
                adapters.push(adapter);
            }
            i += 1;
        }
    }
    adapters
}

fn create_video_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut last = None;
    for attempt in 1..=ATTEMPTS {
        match try_create(None) {
            Ok(created) => return Ok(created),
            Err(e) => {
                tracing::warn!("la GPU rechazó el dispositivo de vídeo (intento {attempt} de {ATTEMPTS}): {e}");
                last = Some(e);
            }
        }
        if attempt < ATTEMPTS {
            std::thread::sleep(RETRY_DELAY);
        }
    }
    for adapter in other_adapters() {
        if let Ok(created) = try_create(Some(&adapter)) {
            tracing::warn!("se usa otra GPU para el vídeo porque la principal no está disponible");
            return Ok(created);
        }
    }
    let e = last.unwrap_or_else(|| Error::Media("sin GPU".into()));
    Err(Error::Media(format!("no se pudo crear el dispositivo D3D11 con vídeo: {e}")))
}

impl GpuDevice {
    pub fn create() -> Result<Arc<Self>> {
        mf::ensure_started()?;
        let (device, context) = create_video_device()?;
        // SAFETY: llamadas COM sobre objetos recién creados.
        let (manager, reset_token) = unsafe {
            let mt: ID3D11Multithread = device.cast()?;
            let _ = mt.SetMultithreadProtected(true);
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| Error::Media(format!("no se pudo crear el gestor DXGI de Media Foundation: {e}")))?;
            let manager = manager.ok_or_else(|| {
                Error::Media("no se pudo crear el gestor de dispositivos DXGI".into())
            })?;
            // El enlace de `windows` tipa el parámetro como IDirect3DDevice9; el real es IUnknown.
            (Interface::vtable(&manager).ResetDevice)(
                Interface::as_raw(&manager),
                Interface::as_raw(&device),
                token,
            )
            .ok()
            .map_err(|e| Error::Media(format!("no se pudo asociar la GPU a Media Foundation: {e}")))?;
            (manager, token)
        };
        Ok(Arc::new(Self {
            device,
            context,
            manager,
            reset_token,
        }))
    }
}

#[cfg(test)]
mod probe {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1};
    use windows::core::Interface;

    // Diagnostic: which adapter / flag combinations can create a D3D11 device.
    #[test]
    #[ignore = "diagnóstico de GPU; ejecutar a mano con --nocapture"]
    fn probe_device_creation() {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
            let mut adapters: Vec<(String, Option<IDXGIAdapter>)> = vec![(String::from("predeterminado"), None)];
            let mut i = 0;
            while let Ok(a) = factory.EnumAdapters1(i) {
                let desc = a.GetDesc1().unwrap();
                let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
                adapters.push((format!("{name} (vram {} MB)", desc.DedicatedVideoMemory / 1_048_576), Some(a.cast().unwrap())));
                i += 1;
            }
            let combos: [(&str, D3D11_CREATE_DEVICE_FLAG); 3] = [
                ("BGRA", D3D11_CREATE_DEVICE_BGRA_SUPPORT),
                ("VIDEO", D3D11_CREATE_DEVICE_VIDEO_SUPPORT),
                ("BGRA|VIDEO", D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT),
            ];
            for (name, adapter) in &adapters {
                for (label, flags) in combos {
                    for levels in [&[D3D_FEATURE_LEVEL_11_1][..], &[D3D_FEATURE_LEVEL_11_0][..]] {
                        let kind = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
                        let mut device = None;
                        let r = D3D11CreateDevice(adapter.as_ref(), kind, HMODULE::default(), flags, Some(levels), D3D11_SDK_VERSION, Some(&mut device), None, None);
                        println!("{name:<45} {label:<11} {:?} -> {:?}", levels[0], r.map(|_| "OK"));
                    }
                }
            }
            let _ = std::any::type_name::<IDXGIAdapter1>();
        }
    }
}
