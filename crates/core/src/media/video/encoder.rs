use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface};

use crate::media::capture::MtGuard;
use crate::media::gpu::GpuDevice;
use crate::media::mf;
use crate::{Error, Result};

const CLSID_H264_SOFTWARE_ENCODER: GUID = GUID::from_u128(0x6ca50344_051a_4ded_9779_a43305165e35);
const INPUT_WAIT: Duration = Duration::from_millis(50);
const OUTPUT_WAIT: Duration = Duration::from_millis(30);
const MAX_CACHED_INPUTS: usize = 8;
const PROFILE_HIGH: u32 = 100;
const PROFILE_MAIN: u32 = 77;
const ANNEXB_START: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub pts: i64,
    pub keyframe: bool,
}

struct NalUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

// Posición del primer cero de un prefijo 00 00 01 a partir de `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from + 2;
    while i < data.len() {
        let off = data[i..].iter().position(|&b| b == 1)?;
        let j = i + off;
        if j >= from + 2 && data[j - 1] == 0 && data[j - 2] == 0 {
            return Some(j - 2);
        }
        i = j + 1;
    }
    None
}

// Unidades NAL de un flujo Annex B, sin prefijo de inicio.
fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits { data, pos: 0 }
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = find_start_code(self.data, self.pos)? + 3;
            let end = find_start_code(self.data, start).unwrap_or(self.data.len());
            self.pos = end;
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    }
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

struct Candidate {
    activate: IMFActivate,
    name: String,
    luid: Option<u64>,
}

fn adapter_luid(gpu: &GpuDevice) -> Option<u64> {
    let dxgi: IDXGIDevice = gpu.device.cast().ok()?;
    // SAFETY: consultas DXGI de solo lectura sobre el dispositivo propio.
    let desc = unsafe { dxgi.GetAdapter().ok()?.GetDesc().ok()? };
    Some(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
}

fn enumerate_hardware() -> Result<Vec<Candidate>> {
    mf::ensure_started()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list = Vec::new();
    let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx devuelve un array CoTaskMem de activadores que se toman uno a uno
    // y cuya memoria se libera al final.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut raw,
            &mut count,
        )?;
        if raw.is_null() {
            return Ok(list);
        }
        for i in 0..count as usize {
            let Some(activate) = std::ptr::read(raw.add(i)) else {
                continue;
            };
            let luid = activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok();
            let mut name = String::from("MFT H.264 desconocida");
            let mut p = windows::core::PWSTR::null();
            let mut len = 0u32;
            if activate
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len)
                .is_ok()
            {
                name = p.to_string().unwrap_or(name);
                CoTaskMemFree(Some(p.0 as *const _));
            }
            list.push(Candidate {
                activate,
                name,
                luid,
            });
        }
        CoTaskMemFree(Some(raw as *const _));
    }
    Ok(list)
}

fn video_type(subtype: &GUID, cfg: &EncoderConfig) -> Result<IMFMediaType> {
    // SAFETY: llamadas COM sobre un tipo de medio recién creado.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

fn set_types(xf: &IMFTransform, cfg: &EncoderConfig) -> Result<u32> {
    let mut failure = None;
    let mut chosen = 0;
    for profile in [PROFILE_HIGH, PROFILE_MAIN] {
        let out = video_type(&MFVideoFormat_H264, cfg)?;
        // SAFETY: atributos sobre un tipo propio y negociación sobre la MFT propia.
        let r = unsafe {
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            xf.SetOutputType(0, &out, 0)
        };
        match r {
            Ok(()) => {
                chosen = profile;
                failure = None;
                break;
            }
            Err(e) => failure = Some(e),
        }
    }
    if let Some(e) = failure {
        return Err(e.into());
    }
    let mut failure = None;
    for with_color in [true, false] {
        let t = video_type(&MFVideoFormat_NV12, cfg)?;
        // SAFETY: igual que arriba; los atributos de color son opcionales.
        let r = unsafe {
            if with_color {
                t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
                t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
                t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            }
            xf.SetInputType(0, &t, 0)
        };
        match r {
            Ok(()) => return Ok(chosen),
            Err(e) => failure = Some(e),
        }
    }
    Err(failure.map_or_else(
        || Error::Media("tipo de entrada rechazado".into()),
        Into::into,
    ))
}

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: VARIANT_BOOL(if v { -1 } else { 0 }),
                },
            }),
        },
    }
}

fn set_codec(codec: &ICodecAPI, key: &GUID, value: VARIANT, what: &str) {
    // SAFETY: el VARIANT vive durante la llamada.
    if let Err(e) = unsafe { codec.SetValue(key, &value) } {
        tracing::debug!("codificador H.264: no se pudo fijar {what}: {e}");
    }
}

fn configure_codec(xf: &IMFTransform, cfg: &EncoderConfig) -> Option<ICodecAPI> {
    let codec: ICodecAPI = xf.cast().ok()?;
    set_codec(
        &codec,
        &CODECAPI_AVLowLatencyMode,
        variant_bool(true),
        "baja latencia",
    );
    set_codec(
        &codec,
        &CODECAPI_AVEncCommonRateControlMode,
        variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
        "control de tasa",
    );
    set_codec(
        &codec,
        &CODECAPI_AVEncCommonMeanBitRate,
        variant_u32(cfg.bitrate),
        "tasa de bits",
    );
    set_codec(
        &codec,
        &CODECAPI_AVEncMPVGOPSize,
        variant_u32(cfg.fps.max(1) * 2),
        "tamaño de GOP",
    );
    set_codec(
        &codec,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        variant_u32(0u32),
        "fotogramas B",
    );
    Some(codec)
}

pub struct H264Encoder {
    pub gpu: Arc<GpuDevice>,
    pub config: EncoderConfig,
    xf: IMFTransform,
    activate: Option<IMFActivate>,
    events: Option<IMFMediaEventGenerator>,
    codec: Option<ICodecAPI>,
    name: String,
    hardware: bool,
    d3d_input: bool,
    out_sample: Option<(IMFSample, IMFMediaBuffer)>,
    need_input: u32,
    have_output: u32,
    frame_duration: i64,
    inputs: Vec<(ID3D11Texture2D, IMFMediaBuffer)>,
    staging: Option<ID3D11Texture2D>,
    out: Vec<u8>,
    out_pts: i64,
    out_key: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
    header: Vec<u8>,
}

// SAFETY: la MFT vive en el apartamento MTA y el dispositivo D3D11 está protegido para
// multihilo; `&mut self` garantiza uso exclusivo desde un solo hilo a la vez.
unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(gpu: Arc<GpuDevice>, config: EncoderConfig) -> Result<Self> {
        if config.width < 2
            || config.height < 2
            || config.width % 2 != 0
            || config.height % 2 != 0
            || config.fps == 0
        {
            return Err(Error::Media(format!(
                "configuración de codificador no válida: {}x{} a {} fps",
                config.width, config.height, config.fps
            )));
        }
        let luid = adapter_luid(&gpu);
        let mut candidates = enumerate_hardware().unwrap_or_else(|e| {
            tracing::warn!("no se pudieron enumerar los codificadores H.264 por hardware: {e}");
            Vec::new()
        });
        candidates.sort_by_key(|c| c.luid.is_none_or(|l| Some(l) != luid));
        for c in candidates {
            match Self::open_hardware(&gpu, config, &c) {
                Ok(enc) => return Ok(enc),
                Err(e) => tracing::warn!("codificador \"{}\" descartado: {e}", c.name),
            }
        }
        tracing::warn!(
            "ningún codificador H.264 por hardware es utilizable: se usa el de software de Microsoft (mayor latencia y consumo de CPU)"
        );
        Self::open_software(gpu, config)
    }

    fn open_hardware(gpu: &Arc<GpuDevice>, config: EncoderConfig, c: &Candidate) -> Result<Self> {
        // SAFETY: activación y configuración de una MFT propia con el gestor DXGI del dispositivo.
        let (xf, is_async) = unsafe {
            let xf: IMFTransform = c.activate.ActivateObject()?;
            let attrs = xf.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            xf.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                Interface::as_raw(&gpu.manager) as usize,
            )?;
            (xf, is_async)
        };
        let profile = set_types(&xf, &config)?;
        let codec = configure_codec(&xf, &config);
        let events = if is_async {
            Some(xf.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };
        tracing::info!(
            "codificador H.264 por hardware \"{}\" ({}x{} {} fps {} bps, perfil {})",
            c.name,
            config.width,
            config.height,
            config.fps,
            config.bitrate,
            if profile == PROFILE_HIGH {
                "High"
            } else {
                "Main"
            }
        );
        Self::from_parts(
            gpu.clone(),
            config,
            xf,
            Some(c.activate.clone()),
            events,
            codec,
            c.name.clone(),
            true,
        )
    }

    fn open_software(gpu: Arc<GpuDevice>, config: EncoderConfig) -> Result<Self> {
        let xf = mf::create_transform(&CLSID_H264_SOFTWARE_ENCODER)?;
        set_types(&xf, &config)?;
        let codec = configure_codec(&xf, &config);
        Self::from_parts(
            gpu,
            config,
            xf,
            None,
            None,
            codec,
            "H264 Encoder MFT (software)".into(),
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        gpu: Arc<GpuDevice>,
        config: EncoderConfig,
        xf: IMFTransform,
        activate: Option<IMFActivate>,
        events: Option<IMFMediaEventGenerator>,
        codec: Option<ICodecAPI>,
        name: String,
        hardware: bool,
    ) -> Result<Self> {
        // SAFETY: mensajes y consultas sobre la MFT propia ya configurada.
        let (out_sample, header_blob) = unsafe {
            xf.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            xf.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let info = xf.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;
            let out_sample = if provides {
                None
            } else {
                let buf = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?;
                let s = MFCreateSample()?;
                s.AddBuffer(&buf)?;
                Some((s, buf))
            };
            let mut blob = Vec::new();
            if let Ok(t) = xf.GetOutputCurrentType(0)
                && let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
            {
                blob.resize(n as usize, 0);
                if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
                    .is_err()
                {
                    blob.clear();
                }
            }
            (out_sample, blob)
        };
        let mut enc = Self {
            gpu,
            config,
            xf,
            activate,
            events,
            codec,
            name,
            hardware,
            d3d_input: hardware,
            out_sample,
            need_input: 0,
            have_output: 0,
            frame_duration: 10_000_000 / config.fps as i64,
            inputs: Vec::new(),
            staging: None,
            out: Vec::with_capacity(512 * 1024),
            out_pts: 0,
            out_key: false,
            sps: Vec::new(),
            pps: Vec::new(),
            header: Vec::new(),
        };
        enc.cache_parameter_sets(&header_blob);
        Ok(enc)
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // Disponible desde la creación si la MFT publica la cabecera de secuencia; si no,
    // tras devolver el primer IDR.
    pub fn sps_pps(&self) -> Option<(&[u8], &[u8])> {
        (!self.sps.is_empty() && !self.pps.is_empty()).then_some((&self.sps[..], &self.pps[..]))
    }

    pub fn request_keyframe(&mut self) {
        if let Some(c) = &self.codec {
            set_codec(
                c,
                &CODECAPI_AVEncVideoForceKeyFrame,
                variant_u32(1u32),
                "fotograma clave",
            );
        }
    }

    // Envía la textura y devuelve la salida disponible: normalmente la del mismo fotograma
    // (hasta ~30 ms de espera acotada); si el codificador aún no la tiene, se devuelve en la
    // siguiente llamada (latencia N+1). Las salidas atrasadas se recogen con `poll_output`.
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        pts: i64,
    ) -> Result<Option<EncodedFrame<'_>>> {
        let sample = self.make_input(texture, pts)?;
        let got = if self.events.is_some() {
            self.encode_async(&sample)?
        } else {
            self.encode_sync(&sample)?
        };
        Ok(got.then(|| self.frame()))
    }

    pub fn poll_output(&mut self) -> Result<Option<EncodedFrame<'_>>> {
        if self.events.is_some() {
            self.pump(Duration::ZERO, |_| false)?;
        }
        let got = self.have_output > 0 && self.take_output()?;
        Ok(got.then(|| self.frame()))
    }

    fn frame(&self) -> EncodedFrame<'_> {
        EncodedFrame {
            data: &self.out,
            pts: self.out_pts,
            keyframe: self.out_key,
        }
    }

    fn encode_async(&mut self, sample: &IMFSample) -> Result<bool> {
        self.pump(Duration::ZERO, |s| s.need_input > 0)?;
        if self.need_input == 0 {
            self.pump(INPUT_WAIT, |s| s.need_input > 0)?;
        }
        if self.need_input == 0 {
            tracing::warn!("el codificador no pidió entrada a tiempo: se descarta el fotograma");
            return if self.have_output > 0 {
                self.take_output()
            } else {
                Ok(false)
            };
        }
        // SAFETY: muestra propia; la MFT asíncrona pidió entrada con METransformNeedInput.
        unsafe { self.xf.ProcessInput(0, sample, 0)? };
        self.need_input -= 1;
        if self.have_output == 0 {
            self.pump(OUTPUT_WAIT, |s| s.have_output > 0)?;
        }
        if self.have_output > 0 {
            self.take_output()
        } else {
            Ok(false)
        }
    }

    fn encode_sync(&mut self, sample: &IMFSample) -> Result<bool> {
        // SAFETY: muestra propia sobre una MFT síncrona.
        let first = unsafe { self.xf.ProcessInput(0, sample, 0) };
        let mut got = false;
        if let Err(e) = first {
            if e.code() != MF_E_NOTACCEPTING {
                return Err(e.into());
            }
            got = self.take_output()?;
            // SAFETY: igual que arriba.
            unsafe { self.xf.ProcessInput(0, sample, 0)? };
        }
        if !got {
            got = self.take_output()?;
        }
        Ok(got)
    }

    fn pump(&mut self, wait: Duration, done: impl Fn(&Self) -> bool) -> Result<()> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        let deadline = Instant::now() + wait;
        let mut spins = 0u32;
        loop {
            loop {
                // SAFETY: lectura sin bloqueo de la cola de eventos de la MFT.
                match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(ev) => self.handle_event(&ev)?,
                    Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if done(self) || Instant::now() >= deadline {
                return Ok(());
            }
            spins += 1;
            if spins < 200 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }

    fn handle_event(&mut self, ev: &IMFMediaEvent) -> Result<()> {
        // SAFETY: consultas de solo lectura sobre un evento recién obtenido.
        let kind = unsafe { ev.GetType()? };
        if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            self.have_output += 1;
        } else if kind == MEError.0 as u32 {
            // SAFETY: igual que arriba.
            let status = unsafe { ev.GetStatus()? };
            return Err(windows::core::Error::from(status).into());
        }
        Ok(())
    }

    fn take_output(&mut self) -> Result<bool> {
        if self.events.is_some() {
            self.have_output = self.have_output.saturating_sub(1);
        }
        if let Some((s, b)) = &self.out_sample {
            // SAFETY: muestra y búfer propios reutilizados entre salidas.
            unsafe {
                s.RemoveAllBuffers()?;
                b.SetCurrentLength(0)?;
                s.AddBuffer(b)?;
            }
        }
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(self.out_sample.as_ref().map(|(s, _)| s.clone())),
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
            Ok(()) => match sample {
                Some(s) => {
                    self.store_output(&s)?;
                    Ok(true)
                }
                None => Ok(false),
            },
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // SAFETY: renegociación del tipo de salida.
                unsafe {
                    let t = self.xf.GetOutputAvailableType(0, 0)?;
                    self.xf.SetOutputType(0, &t, 0)?;
                }
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn store_output(&mut self, sample: &IMFSample) -> Result<()> {
        // SAFETY: el búfer se bloquea solo mientras se copia al Vec reutilizado.
        unsafe {
            self.out_pts = sample.GetSampleTime().unwrap_or(0);
            let buf = sample.ConvertToContiguousBuffer()?;
            let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
            buf.Lock(&mut p, None, Some(&mut len))?;
            self.fill_output(std::slice::from_raw_parts(p, len as usize));
            buf.Unlock()?;
        }
        Ok(())
    }

    fn cache_parameter_sets(&mut self, data: &[u8]) -> (bool, bool, bool) {
        let (mut has_sps, mut has_pps, mut idr) = (false, false, false);
        let mut changed = false;
        for nal in nal_units(data) {
            match nal[0] & 0x1f {
                7 => {
                    has_sps = true;
                    if self.sps != nal {
                        self.sps.clear();
                        self.sps.extend_from_slice(nal);
                        changed = true;
                    }
                }
                8 => {
                    has_pps = true;
                    if self.pps != nal {
                        self.pps.clear();
                        self.pps.extend_from_slice(nal);
                        changed = true;
                    }
                }
                5 => idr = true,
                _ => {}
            }
        }
        if changed && !self.sps.is_empty() && !self.pps.is_empty() {
            self.header.clear();
            self.header.extend_from_slice(&ANNEXB_START);
            self.header.extend_from_slice(&self.sps);
            self.header.extend_from_slice(&ANNEXB_START);
            self.header.extend_from_slice(&self.pps);
        }
        (has_sps, has_pps, idr)
    }

    fn fill_output(&mut self, data: &[u8]) {
        let (has_sps, has_pps, idr) = self.cache_parameter_sets(data);
        self.out.clear();
        if idr && !(has_sps && has_pps) && !self.header.is_empty() {
            self.out.extend_from_slice(&self.header);
        }
        self.out.extend_from_slice(data);
        self.out_key = idr;
    }

    fn make_input(&mut self, texture: &ID3D11Texture2D, pts: i64) -> Result<IMFSample> {
        let buf = if self.d3d_input {
            self.surface_buffer(texture)?
        } else {
            self.readback_buffer(texture)?
        };
        // SAFETY: muestra nueva que envuelve el búfer sin copiarlo.
        unsafe {
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(pts)?;
            s.SetSampleDuration(self.frame_duration)?;
            Ok(s)
        }
    }

    fn check_texture(&self, texture: &ID3D11Texture2D) -> Result<()> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: descripción de una textura del llamador.
        unsafe { texture.GetDesc(&mut d) };
        if d.Format != DXGI_FORMAT_NV12
            || d.Width != self.config.width
            || d.Height != self.config.height
        {
            return Err(Error::Media(format!(
                "la textura de entrada ({}x{}, formato {}) no coincide con el codificador ({}x{} NV12)",
                d.Width, d.Height, d.Format.0, self.config.width, self.config.height
            )));
        }
        Ok(())
    }

    fn surface_buffer(&mut self, texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        if let Some((_, b)) = self
            .inputs
            .iter()
            .find(|(t, _)| t.as_raw() == texture.as_raw())
        {
            return Ok(b.clone());
        }
        self.check_texture(texture)?;
        // SAFETY: el búfer envuelve la textura sin copiarla; la caché conserva una referencia.
        let buf = unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let len = buf
                .cast::<IMF2DBuffer>()
                .and_then(|b| b.GetContiguousLength())
                .unwrap_or(self.config.width * self.config.height * 3 / 2);
            buf.SetCurrentLength(len)?;
            buf
        };
        if self.inputs.len() >= MAX_CACHED_INPUTS {
            self.inputs.remove(0);
        }
        self.inputs.push((texture.clone(), buf.clone()));
        Ok(buf)
    }

    // Ruta de respaldo por software: lectura de la GPU a memoria del sistema.
    fn readback_buffer(&mut self, texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        self.check_texture(texture)?;
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.config.width,
                Height: self.config.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut t = None;
            // SAFETY: descripción válida y puntero de salida local.
            unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut t))? };
            self.staging = t;
        }
        let staging = self
            .staging
            .clone()
            .ok_or_else(|| Error::Media("no se pudo crear la textura de lectura".into()))?;
        let _lock = MtGuard::new(&self.gpu.device);
        // SAFETY: la textura de lectura se mapea solo mientras se copian las filas al búfer.
        unsafe {
            self.gpu.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.gpu
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            let buf = MFCreateMemoryBuffer((w * h * 3 / 2) as u32)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            for y in 0..h + h / 2 {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * w), w);
            }
            buf.Unlock()?;
            self.gpu.context.Unmap(&staging, 0);
            buf.SetCurrentLength((w * h * 3 / 2) as u32)?;
            Ok(buf)
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: cierre ordenado de una MFT propia; los errores no son accionables aquí.
        unsafe {
            let _ = self.xf.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.xf.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.xf.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(a) = &self.activate {
                let _ = a.ShutdownObject();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capture::DesktopDuplicator;
    use crate::media::video::H264Decoder;

    #[test]
    fn splits_annexb_with_mixed_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0,
        ];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]
        );
    }

    #[test]
    fn empty_and_garbage_yield_no_nals() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[1, 2, 3, 0, 0]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
    }

    #[test]
    fn start_code_at_buffer_edges() {
        assert_eq!(find_start_code(&[0, 0, 1, 5], 0), Some(0));
        assert_eq!(find_start_code(&[7, 0, 0, 1, 5], 0), Some(1));
        assert_eq!(find_start_code(&[0, 1, 0, 0, 1], 0), Some(2));
        assert_eq!(find_start_code(&[0, 0, 1, 0, 0, 1], 1), Some(3));
    }

    #[test]
    #[ignore = "requiere GPU y escritorio interactivo"]
    fn capture_encode_decode_roundtrip() {
        let gpu = GpuDevice::create().expect("dispositivo GPU");
        let cfg = EncoderConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 6_000_000,
        };
        let mut cap =
            DesktopDuplicator::new(gpu.clone(), 0, cfg.width, cfg.height).expect("captura");
        let mut enc = H264Encoder::new(gpu.clone(), cfg).expect("codificador");
        eprintln!("codificador: {} hardware={}", enc.name(), enc.is_hardware());
        assert!(enc.is_hardware());
        let mut dec = H264Decoder::new(gpu).expect("decodificador");
        let start = Instant::now();
        let (mut produced, mut decoded, mut captured, mut bytes) = (0u32, 0u32, 0u32, 0usize);
        let (mut enc_time, mut cap_time) = (Duration::ZERO, Duration::ZERO);
        let mut first = true;
        let mut keyframes = 0;
        let mut next_tick = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let t = Instant::now();
            let fresh = cap.next_frame(Duration::from_millis(33)).expect("captura");
            let frame = match fresh {
                Some(f) => {
                    captured += 1;
                    cap_time += t.elapsed();
                    f
                }
                None => match cap.last_frame() {
                    Some(f) => f,
                    None => continue,
                },
            };
            let t = Instant::now();
            let out = enc.encode(frame.nv12, frame.pts).expect("codificar");
            let dt = t.elapsed();
            if let Some(au) = out {
                enc_time += dt;
                produced += 1;
                bytes += au.data.len();
                keyframes += au.keyframe as u32;
                if first {
                    first = false;
                    assert!(au.keyframe, "el primer fotograma debe ser IDR");
                    let kinds: Vec<u8> = nal_units(au.data).map(|n| n[0] & 0x1f).collect();
                    assert!(
                        kinds.contains(&7) && kinds.contains(&8),
                        "faltan SPS/PPS: {kinds:?}"
                    );
                    assert!(au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]));
                }
                if let Some(f) = dec.decode(au.data, au.pts).expect("decodificar") {
                    assert_eq!((f.width, f.height), (1280, 720));
                    decoded += 1;
                }
            }
            next_tick += Duration::from_millis(33);
            if let Some(w) = next_tick.checked_duration_since(Instant::now()) {
                std::thread::sleep(w);
            }
        }
        let total = start.elapsed();
        assert!(enc.sps_pps().is_some());
        eprintln!(
            "fotogramas codificados={produced} capturados_nuevos={captured} decodificados={decoded} claves={keyframes} bytes={bytes} ({:.2} Mbps)",
            bytes as f64 * 8.0 / total.as_secs_f64() / 1e6
        );
        eprintln!(
            "latencia media de codificación={:?} captura+blit media={:?} caudal={:.1} fps",
            enc_time / produced.max(1),
            cap_time / captured.max(1),
            produced as f64 / total.as_secs_f64()
        );
        assert!(produced >= 30, "pocos fotogramas: {produced}");
        assert!(decoded >= produced - 3);
    }
}
