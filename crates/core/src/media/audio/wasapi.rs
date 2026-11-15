use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::Result;

pub(super) struct Event(pub HANDLE);

// SAFETY: un HANDLE de evento puede usarse y cerrarse desde cualquier hilo.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn new() -> Result<Self> {
        // SAFETY: evento anónimo de reinicio automático.
        Ok(Self(unsafe { CreateEventW(None, false, false, None) }?))
    }

    pub fn set(&self) {
        // SAFETY: el handle es válido mientras exista `self`.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: handle propio.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) fn format_44k() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 1,
        nChannels: 2,
        nSamplesPerSec: super::SAMPLE_RATE,
        nAvgBytesPerSec: super::SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

pub(super) fn default_device(flow: EDataFlow) -> Result<IMMDevice> {
    // SAFETY: creación COM estándar del enumerador de dispositivos.
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        Ok(e.GetDefaultAudioEndpoint(flow, eConsole)?)
    }
}

pub(super) fn is_invalidated(e: &windows::core::Error) -> bool {
    e.code() == AUDCLNT_E_DEVICE_INVALIDATED || e.code() == AUDCLNT_E_RESOURCES_INVALIDATED
}
