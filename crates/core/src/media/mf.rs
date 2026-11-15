use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::GUID;

use crate::{Error, Result};

pub(crate) const MF_AUDIO_FORMAT_ALAC: GUID =
    GUID::from_u128(0x63616c61_0000_0010_8000_00aa00389b71);
pub(crate) const CLSID_ALAC_DECODER: GUID = GUID::from_u128(0xc0cd7d12_31fc_4bbc_b363_7322ee3e1879);
#[cfg(test)]
pub(crate) const CLSID_ALAC_ENCODER: GUID = GUID::from_u128(0x9ab6a28c_748e_4b6a_bfff_cc443b8e8fb4);

const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;

pub(crate) fn ensure_com() -> Result<()> {
    // SAFETY: inicialización COM por hilo; sin punteros implicados.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() && hr.0 != RPC_E_CHANGED_MODE {
        return Err(windows::core::Error::from(hr).into());
    }
    Ok(())
}

pub(crate) fn ensure_started() -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    ensure_com()?;
    STARTED
        .get_or_init(|| {
            // SAFETY: MFStartup se llama una sola vez por proceso.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| Error::Media(format!("no se pudo iniciar Media Foundation: {e}")))
}

pub(crate) fn create_transform(clsid: &GUID) -> Result<IMFTransform> {
    ensure_started()?;
    // SAFETY: CoCreateInstance con un CLSID de MFT en proceso.
    let xf = unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) }?;
    Ok(xf)
}

pub(crate) fn pcm_type(sample_rate: u32, channels: u32) -> Result<IMFMediaType> {
    // SAFETY: llamadas COM sobre un tipo de medio recién creado.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 2 * channels)?;
        t.SetUINT32(
            &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
            2 * channels * sample_rate,
        )?;
        Ok(t)
    }
}

pub(crate) struct Mft {
    xf: IMFTransform,
    in_sample: IMFSample,
    in_buf: IMFMediaBuffer,
    in_cap: u32,
    out_sample: Option<IMFSample>,
    out_buf: Option<IMFMediaBuffer>,
    out_type: Option<IMFMediaType>,
    step_100ns: i64,
    next_time: i64,
}

// SAFETY: la MFT se crea en un hilo MTA y los objetos COM de un apartamento MTA
// pueden usarse desde cualquier hilo MTA; el `&mut self` garantiza uso exclusivo.
unsafe impl Send for Mft {}

impl Mft {
    // Debe llamarse con los tipos de entrada y salida ya negociados.
    pub(crate) fn new(
        xf: IMFTransform,
        out_type: Option<IMFMediaType>,
        in_cap: u32,
        out_cap: u32,
        step_100ns: i64,
    ) -> Result<Self> {
        // SAFETY: llamadas COM sobre objetos propios; los búferes se crean una sola vez.
        unsafe {
            xf.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let in_buf = MFCreateMemoryBuffer(in_cap)?;
            let in_sample = MFCreateSample()?;
            in_sample.AddBuffer(&in_buf)?;
            let info = xf.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let (out_sample, out_buf) = if provides {
                (None, None)
            } else {
                let buf = MFCreateMemoryBuffer(out_cap.max(info.cbSize))?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                (Some(s), Some(buf))
            };
            Ok(Self {
                xf,
                in_sample,
                in_buf,
                in_cap,
                out_sample,
                out_buf,
                out_type,
                step_100ns,
                next_time: 0,
            })
        }
    }

    pub(crate) fn process(
        &mut self,
        input: &[u8],
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        if input.len() > self.in_cap as usize {
            return Err(Error::Media("paquete de audio demasiado grande".into()));
        }
        // SAFETY: el búfer de entrada tiene capacidad suficiente (comprobada arriba)
        // y se bloquea durante la copia.
        unsafe {
            let mut p = std::ptr::null_mut();
            self.in_buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(input.as_ptr(), p, input.len());
            self.in_buf.Unlock()?;
            self.in_buf.SetCurrentLength(input.len() as u32)?;
            self.in_sample.SetSampleTime(self.next_time)?;
            self.in_sample.SetSampleDuration(self.step_100ns)?;
            self.next_time += self.step_100ns;
            if let Err(e) = self.xf.ProcessInput(0, &self.in_sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(e.into());
                }
                self.drain(sink)?;
                self.xf.ProcessInput(0, &self.in_sample, 0)?;
            }
        }
        self.drain(sink)
    }

    #[cfg(test)]
    pub(crate) fn finish(&mut self, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        // SAFETY: mensaje de vaciado sobre una MFT propia.
        unsafe { self.xf.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        self.drain(sink)
    }

    fn drain(&mut self, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        loop {
            if let (Some(s), Some(b)) = (&self.out_sample, &self.out_buf) {
                // SAFETY: muestra y búfer propios; algunas MFT añaden sus propios búferes.
                unsafe {
                    s.RemoveAllBuffers()?;
                    b.SetCurrentLength(0)?;
                    s.AddBuffer(b)?;
                }
            }
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.out_sample.clone()),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: `out` vive durante la llamada; las referencias COM se liberan abajo.
            let res = unsafe { self.xf.ProcessOutput(0, &mut out, &mut status) };
            let [o] = out;
            let sample = ManuallyDrop::into_inner(o.pSample);
            drop(ManuallyDrop::into_inner(o.pEvents));
            match res {
                Ok(()) => {
                    let Some(sample) = sample else { continue };
                    // SAFETY: bloqueo de cada búfer mientras se lee.
                    unsafe {
                        for i in 0..sample.GetBufferCount()? {
                            let buf = sample.GetBufferByIndex(i)?;
                            let mut p = std::ptr::null_mut();
                            let mut len = 0u32;
                            buf.Lock(&mut p, None, Some(&mut len))?;
                            let r = if len > 0 {
                                sink(std::slice::from_raw_parts(p, len as usize))
                            } else {
                                Ok(())
                            };
                            buf.Unlock()?;
                            r?;
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: renegociación del tipo de salida.
                    unsafe {
                        let t = match &self.out_type {
                            Some(t) => t.clone(),
                            None => self.xf.GetOutputAvailableType(0, 0)?,
                        };
                        self.xf.SetOutputType(0, &t, 0)?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub(crate) const CLSID_AAC_DECODER: GUID = GUID::from_u128(0x32d186a7_218f_4c75_8876_dd77273a8999);
#[cfg(test)]
pub(crate) const CLSID_AAC_ENCODER: GUID = GUID::from_u128(0x93af0c51_2275_45d2_a35b_f2ba21caed00);
