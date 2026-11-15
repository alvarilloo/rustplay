use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{E_NOINTERFACE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL};
use windows::Win32::System::Threading::{GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use super::wasapi::{Event, default_device, format_44k};
use crate::media::mf::ensure_com;
use crate::{Error, Result};

const FLAGS: u32 = AUDCLNT_STREAMFLAGS_LOOPBACK
    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
const EXCLUDE_OWN_TREE: PROCESS_LOOPBACK_MODE = PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE;
const WAIT_MS: u32 = 20;
const ACTIVATION_TIMEOUT_MS: u32 = 3000;
const PENDING_CAPACITY: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Process,
    Endpoint,
}

// Objeto COM manual: `#[implement]` exige una dependencia directa de `windows-core`.
#[repr(C)]
struct ActivationHandler {
    vtbl: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
    refs: AtomicU32,
    done: Arc<Event>,
}

const IID_AGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

static HANDLER_VTBL: IActivateAudioInterfaceCompletionHandler_Vtbl =
    IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: IUnknown_Vtbl {
            QueryInterface: handler_query,
            AddRef: handler_add_ref,
            Release: handler_release,
        },
        ActivateCompleted: handler_completed,
    };

unsafe extern "system" fn handler_query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: COM garantiza punteros válidos; `this` es un `ActivationHandler`.
    unsafe {
        let iid = *iid;
        if iid == IUnknown::IID
            || iid == IActivateAudioInterfaceCompletionHandler::IID
            || iid == IID_AGILE_OBJECT
        {
            handler_add_ref(this);
            *out = this;
            HRESULT(0)
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn handler_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` es un `ActivationHandler` vivo.
    unsafe {
        (*(this as *const ActivationHandler))
            .refs
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn handler_release(this: *mut c_void) -> u32 {
    // SAFETY: `this` proviene de `Box::into_raw`; se libera al llegar la cuenta a cero.
    unsafe {
        let left = (*(this as *const ActivationHandler))
            .refs
            .fetch_sub(1, Ordering::Release)
            - 1;
        if left == 0 {
            drop(Box::from_raw(this as *mut ActivationHandler));
        }
        left
    }
}

unsafe extern "system" fn handler_completed(this: *mut c_void, _op: *mut c_void) -> HRESULT {
    // SAFETY: `this` es un `ActivationHandler` vivo.
    unsafe { (*(this as *const ActivationHandler)).done.set() };
    HRESULT(0)
}

fn new_handler(done: Arc<Event>) -> IActivateAudioInterfaceCompletionHandler {
    let raw = Box::into_raw(Box::new(ActivationHandler {
        vtbl: &HANDLER_VTBL,
        refs: AtomicU32::new(1),
        done,
    }));
    // SAFETY: el objeto sigue el diseño COM (puntero a vtable primero) y cede su única referencia.
    unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(raw as *mut c_void) }
}

pub struct LoopbackCapture {
    mode: CaptureMode,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    pending: Vec<i16>,
    pending_pos: usize,
}

// SAFETY: los objetos COM se crean en un apartamento MTA y solo se usan con `&mut self`.
unsafe impl Send for LoopbackCapture {}

impl LoopbackCapture {
    pub fn start() -> Result<Self> {
        match Self::start_mode(CaptureMode::Process) {
            Ok(capture) => {
                tracing::info!(
                    "Captura de audio: todas las aplicaciones salvo RustPlay (independiente del volumen de Windows)"
                );
                Ok(capture)
            }
            Err(e) => {
                tracing::warn!(
                    "La captura por proceso no está disponible ({e}); se usa la mezcla del dispositivo"
                );
                let capture = Self::start_mode(CaptureMode::Endpoint)?;
                tracing::info!(
                    "Captura de audio: mezcla del dispositivo predeterminado (sigue el volumen de Windows)"
                );
                Ok(capture)
            }
        }
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    fn start_mode(mode: CaptureMode) -> Result<Self> {
        Self::start_with(mode, None)
    }

    fn start_with(mode: CaptureMode, target: Option<(u32, PROCESS_LOOPBACK_MODE)>) -> Result<Self> {
        ensure_com()?;
        let (client, buffer_duration) = match mode {
            CaptureMode::Process => {
                // SAFETY: consulta trivial del PID propio.
                let own = (unsafe { GetCurrentProcessId() }, EXCLUDE_OWN_TREE);
                (activate_process_client(target.unwrap_or(own))?, 0)
            }
            CaptureMode::Endpoint => {
                let device = default_device(eRender)?;
                // SAFETY: activación COM estándar sobre un dispositivo propio.
                (unsafe { device.Activate(CLSCTX_ALL, None)? }, 1_000_000)
            }
        };
        let fmt = format_44k();
        // SAFETY: llamadas COM sobre objetos propios; `fmt` vive durante las llamadas.
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                FLAGS,
                buffer_duration,
                0,
                &fmt,
                None,
            )?;
            let event = Event::new()?;
            client.SetEventHandle(event.0)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self {
                mode,
                client,
                capture,
                event,
                pending: Vec::with_capacity(PENDING_CAPACITY),
                pending_pos: 0,
            })
        }
    }

    // Devuelve los fotogramas escritos; 0 si no hay audio reproduciéndose.
    pub fn read(&mut self, out: &mut [i16]) -> Result<usize> {
        let mut written = self.take_pending(out);
        // SAFETY: el búfer de GetBuffer es válido para `frames * 4` bytes hasta ReleaseBuffer.
        unsafe {
            if written == 0 && WaitForSingleObject(self.event.0, WAIT_MS) != WAIT_OBJECT_0 {
                return Ok(0);
            }
            while written < out.len() && self.pending_pos >= self.pending.len() {
                if self.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let samples = frames as usize * 2;
                let src = std::slice::from_raw_parts(data as *const i16, samples);
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let room = out.len() - written;
                let direct = samples.min(room);
                if silent {
                    out[written..written + direct].fill(0);
                } else {
                    out[written..written + direct].copy_from_slice(&src[..direct]);
                }
                written += direct;
                self.pending.clear();
                self.pending_pos = 0;
                if direct < samples {
                    if silent {
                        self.pending.resize(samples - direct, 0);
                    } else {
                        self.pending.extend_from_slice(&src[direct..]);
                    }
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(written / 2)
    }

    fn take_pending(&mut self, out: &mut [i16]) -> usize {
        let avail = self.pending.len() - self.pending_pos;
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + n]);
        self.pending_pos += n;
        n
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        // SAFETY: cliente propio.
        let _ = unsafe { self.client.Stop() };
    }
}

fn activate_process_client(
    (pid, loopback_mode): (u32, PROCESS_LOOPBACK_MODE),
) -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        ..Default::default()
    };
    params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: pid,
        ProcessLoopbackMode: loopback_mode,
    };
    let mut prop = PROPVARIANT::default();
    prop.Anonymous.Anonymous = std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
        vt: VT_BLOB,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: PROPVARIANT_0_0_0 {
            blob: BLOB {
                cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: &mut params as *mut _ as *mut u8,
            },
        },
    });
    let done = Arc::new(Event::new()?);
    let handler = new_handler(done.clone());
    // SAFETY: `params` y `prop` viven hasta que la llamada devuelve; el PROPVARIANT no se
    // libera porque su blob apunta a la pila.
    let op = unsafe {
        let r = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        );
        std::mem::forget(prop);
        r?
    };
    // SAFETY: espera sobre un evento propio.
    if unsafe { WaitForSingleObject(done.0, ACTIVATION_TIMEOUT_MS) } != WAIT_OBJECT_0 {
        return Err(Error::Media(
            "tiempo de espera agotado al activar la captura por proceso".into(),
        ));
    }
    let mut hr = HRESULT(0);
    let mut unknown = None;
    // SAFETY: punteros de salida válidos durante la llamada.
    unsafe { op.GetActivateResult(&mut hr, &mut unknown)? };
    hr.ok()?;
    let unknown = unknown.ok_or_else(|| Error::Media("activación sin interfaz".into()))?;
    Ok(unknown.cast::<IAudioClient>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::audio::Renderer;
    use std::time::{Duration, Instant};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;

    const CHILD_ENV: &str = "RUSTPLAY_LOOPBACK_TONE_CHILD";
    const AMPLITUDE: f32 = 0.15;

    #[test]
    #[ignore]
    fn tone_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let mut renderer = Renderer::start(100).unwrap();
        let end = Instant::now() + Duration::from_secs(6);
        let mut n = 0usize;
        let mut chunk = [0i16; 882];
        while Instant::now() < end {
            if renderer.free_frames() >= 441 {
                for f in 0..441 {
                    let t = (n + f) as f32 / 44_100.0;
                    let s = (AMPLITUDE * 32767.0 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin())
                        as i16;
                    chunk[f * 2] = s;
                    chunk[f * 2 + 1] = s;
                }
                renderer.push(&chunk);
                n += 441;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn measure(
        mode: CaptureMode,
        target: Option<(u32, PROCESS_LOOPBACK_MODE)>,
    ) -> (i16, f64, usize) {
        let mut cap = LoopbackCapture::start_with(mode, target).unwrap();
        let mut buf = [0i16; 4096];
        let (mut peak, mut sum, mut count) = (0i16, 0f64, 0usize);
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            let frames = cap.read(&mut buf).unwrap();
            for &x in &buf[..frames * 2] {
                peak = peak.max(x.saturating_abs());
                sum += (x as f64).powi(2);
                count += 1;
            }
        }
        (peak, (sum / count.max(1) as f64).sqrt(), count / 2)
    }

    #[test]
    #[ignore]
    fn process_vs_endpoint_level() {
        ensure_com().unwrap();
        // SAFETY: lectura de volumen sobre el dispositivo predeterminado.
        unsafe {
            let device = default_device(eRender).unwrap();
            let vol: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).unwrap();
            println!(
                "volumen maestro = {:.3}, silenciado = {}",
                vol.GetMasterVolumeLevelScalar().unwrap(),
                vol.GetMute().unwrap().as_bool()
            );
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "media::audio::loopback::tests::tone_child",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        // El hijo pertenece a nuestro árbol, así que el modo de exclusión lo omite a propósito.
        let excluded = measure(CaptureMode::Process, None);
        let process = measure(
            CaptureMode::Process,
            Some((
                child.id(),
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            )),
        );
        let endpoint = measure(CaptureMode::Endpoint, None);
        let _ = child.kill();
        let _ = child.wait();
        println!(
            "tono 1 kHz a {AMPLITUDE} FS (pico esperado {})",
            (AMPLITUDE * 32767.0) as i32
        );
        println!(
            "proceso (excluye nuestro árbol, el tono debe faltar): pico={} rms={:.1}",
            excluded.0, excluded.1
        );
        println!(
            "proceso (incluye al hijo): pico={} rms={:.1} fotogramas={}",
            process.0, process.1, process.2
        );
        println!(
            "endpoint: pico={} rms={:.1} fotogramas={}",
            endpoint.0, endpoint.1, endpoint.2
        );
        assert!(process.0 > 100, "la captura por proceso no recibió señal");
    }
}
