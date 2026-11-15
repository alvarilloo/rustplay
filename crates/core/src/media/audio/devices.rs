use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::core::{HSTRING, PWSTR};

use crate::Result;
use crate::media::mf::ensure_com;

// A WASAPI render endpoint: `id` is the stable endpoint ID string, `name` its friendly name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    ensure_com()?;
    // SAFETY: creación COM estándar del enumerador de dispositivos.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?)
}

fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: cadena terminada en cero reservada por COM; se libera después de copiarla.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

// Active speakers and headphones, in the order Windows lists them.
pub fn render_devices() -> Result<Vec<AudioDevice>> {
    let enumerator = enumerator()?;
    let mut out = Vec::new();
    // SAFETY: llamadas COM sobre objetos propios.
    unsafe {
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..collection.GetCount()? {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(id) = device.GetId() else { continue };
            let id = take_pwstr(id);
            let name = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .map(|value| value.to_string())
                .ok()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| id.clone());
            out.push(AudioDevice { id, name });
        }
    }
    Ok(out)
}

// The endpoint with this ID when it is still plugged in and enabled.
pub(super) fn active_device(id: &str) -> Option<IMMDevice> {
    let enumerator = enumerator().ok()?;
    // SAFETY: llamadas COM sobre objetos propios; el HSTRING vive durante la llamada.
    unsafe {
        let device = enumerator.GetDevice(&HSTRING::from(id)).ok()?;
        (device.GetState().ok()? == DEVICE_STATE_ACTIVE).then_some(device)
    }
}
