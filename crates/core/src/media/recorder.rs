// Records a received H.264 stream into MP4 files without re-encoding (Media Foundation sink
// writer in passthrough), with the played PCM audio encoded to AAC by the same sink writer.
// All file work runs on a dedicated thread; producers only copy into pooled buffers.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{GUID, HSTRING};

use super::mf;
use crate::{Error, Result};

const QUEUE: usize = 96;
const VIDEO_POOL: usize = 48;
const AUDIO_POOL: usize = 96;
const VIDEO_BUFFER: usize = 256 * 1024;
const AUDIO_BUFFER: usize = 4096 * 2;
const TICKS: i64 = 10_000_000;
// Frames recorded before the start (since the last keyframe) are squeezed 1 ms apart.
const PREROLL_STEP: i64 = 10_000;
const FRAME_DURATION: i64 = TICKS / 60;
// Audio that falls this far behind the wall clock (the sender paused it) is padded with silence.
const AUDIO_GAP: i64 = TICKS / 5;
const AAC_BYTES_PER_SECOND: u32 = 24_000;
const IDLE_WAIT: Duration = Duration::from_millis(50);
// Bytes kept since the last keyframe so a recording can start between keyframes.
pub const GOP_CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
}

// Units seen since the last keyframe, given to a recorder that starts between keyframes.
#[derive(Debug, Clone, Default)]
pub struct Preroll {
    data: Vec<u8>,
    frames: Vec<(usize, usize, i64)>,
    params: Vec<u8>,
}

impl Preroll {
    pub fn frames(&self) -> usize {
        self.frames.len()
    }
}

pub struct RecorderConfig {
    pub folder: PathBuf,
    // File name without extension; later files (after a resolution change) get " (2)", " (3)"...
    pub stem: String,
    pub audio: Option<AudioSpec>,
    pub preroll: Option<Preroll>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderEvent {
    // A file was opened and is being written.
    Started(PathBuf),
    // A file was completed.
    Saved(PathBuf),
    Failed(String),
}

type Notify = Box<dyn Fn(RecorderEvent) + Send>;

enum Msg {
    Video(Vec<u8>, i64),
    Audio(Vec<i16>, Instant),
}

struct Shared {
    stop: AtomicBool,
    video_pool: Mutex<Vec<Vec<u8>>>,
    audio_pool: Mutex<Vec<Vec<i16>>>,
    dropped_video: AtomicU64,
    dropped_audio: AtomicU64,
}

pub struct Recorder {
    tx: SyncSender<Msg>,
    shared: Arc<Shared>,
    started: Instant,
    audio: Option<AudioSpec>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Recorder {
    // Starts the recording thread; files are created when the first keyframe arrives.
    pub fn start(config: RecorderConfig, notify: impl Fn(RecorderEvent) + Send + 'static) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            video_pool: Mutex::new((0..VIDEO_POOL).map(|_| Vec::with_capacity(VIDEO_BUFFER)).collect()),
            audio_pool: Mutex::new((0..AUDIO_POOL).map(|_| Vec::with_capacity(AUDIO_BUFFER)).collect()),
            dropped_video: AtomicU64::new(0),
            dropped_audio: AtomicU64::new(0),
        });
        let audio = config.audio;
        let thread_shared = Arc::clone(&shared);
        let notify: Notify = Box::new(notify);
        let thread = std::thread::Builder::new()
            .name("rustplay-recorder".into())
            .spawn(move || run(config, &rx, &thread_shared, &notify))
            .map_err(|e| Error::Media(format!("no se pudo crear el hilo de grabación: {e}")))?;
        Ok(Self { tx, shared, started: Instant::now(), audio, thread: Mutex::new(Some(thread)) })
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn audio(&self) -> Option<AudioSpec> {
        self.audio
    }

    // Queues an Annex-B access unit; `pts` is in 100 ns units on any clock. Never blocks:
    // returns false (and counts a drop) when the queue is full.
    pub fn push_video(&self, unit: &[u8], pts: i64) -> bool {
        if self.shared.stop.load(Ordering::Acquire) {
            return false;
        }
        let buf = self.shared.video_pool.lock().ok().and_then(|mut pool| pool.pop());
        let Some(mut buf) = buf else {
            self.dropped(&self.shared.dropped_video, "vídeo");
            return false;
        };
        buf.clear();
        buf.extend_from_slice(unit);
        match self.tx.try_send(Msg::Video(buf, pts)) {
            Ok(()) => true,
            Err(TrySendError::Full(Msg::Video(buf, _)) | TrySendError::Disconnected(Msg::Video(buf, _))) => {
                recycle_video(&self.shared, buf);
                self.dropped(&self.shared.dropped_video, "vídeo");
                false
            }
            Err(_) => false,
        }
    }

    // Queues interleaved 16-bit PCM in the format given at start. Never blocks.
    pub fn push_audio(&self, pcm: &[i16]) -> bool {
        if self.audio.is_none() || pcm.is_empty() || self.shared.stop.load(Ordering::Acquire) {
            return false;
        }
        let buf = self.shared.audio_pool.lock().ok().and_then(|mut pool| pool.pop());
        let Some(mut buf) = buf else {
            self.dropped(&self.shared.dropped_audio, "audio");
            return false;
        };
        buf.clear();
        buf.extend_from_slice(pcm);
        match self.tx.try_send(Msg::Audio(buf, Instant::now())) {
            Ok(()) => true,
            Err(TrySendError::Full(Msg::Audio(buf, _)) | TrySendError::Disconnected(Msg::Audio(buf, _))) => {
                recycle_audio(&self.shared, buf);
                self.dropped(&self.shared.dropped_audio, "audio");
                false
            }
            Err(_) => false,
        }
    }

    fn dropped(&self, counter: &AtomicU64, what: &str) {
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 100 == 0 {
            tracing::warn!("grabación: cola llena, se descarta {what} ({n} en total)");
        }
    }

    // Asks the thread to write what is queued and close the file. Does not wait.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.lock().ok().is_none_or(|t| t.as_ref().is_none_or(JoinHandle::is_finished))
    }

    // Stops and waits until the files are closed.
    pub fn finish(&self) {
        self.stop();
        let thread = self.thread.lock().ok().and_then(|mut t| t.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.finish();
    }
}

fn recycle_video(shared: &Shared, buf: Vec<u8>) {
    if let Ok(mut pool) = shared.video_pool.lock() {
        pool.push(buf);
    }
}

fn recycle_audio(shared: &Shared, buf: Vec<i16>) {
    if let Ok(mut pool) = shared.audio_pool.lock() {
        pool.push(buf);
    }
}

// ------------------------------------------------------------------------------------ H.264

// NAL units of an Annex-B buffer, without start codes.
pub(crate) fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        loop {
            let start = next_start(data, pos)?;
            let end = next_start(data, start).map_or(data.len(), |next| {
                // The zero before a 4-byte start code belongs to the next prefix.
                let mut end = next - 3;
                while end > start && data[end - 1] == 0 {
                    end -= 1;
                }
                end
            });
            pos = end.max(start);
            if end > start {
                return Some(&data[start..end]);
            }
            if pos >= data.len() {
                return None;
            }
        }
    })
}

// Index just after the next 00 00 01 at or after `from`.
fn next_start(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i + 3);
        } else {
            i += 1;
        }
    }
    None
}

struct Units<'a> {
    sps: Option<&'a [u8]>,
    pps: Option<&'a [u8]>,
    idr: bool,
}

fn inspect(unit: &[u8]) -> Units<'_> {
    let mut out = Units { sps: None, pps: None, idr: false };
    for nal in nal_units(unit) {
        match nal[0] & 0x1F {
            7 if out.sps.is_none() => out.sps = Some(nal),
            8 if out.pps.is_none() => out.pps = Some(nal),
            5 => out.idr = true,
            _ => {}
        }
    }
    out
}

// True when the access unit holds an IDR picture.
// Runs for every received unit, so it reads only the start: the receiver puts SPS and PPS
// (a few dozen bytes) right before an IDR, which is therefore always inside the first bytes.
pub fn is_keyframe(unit: &[u8]) -> bool {
    inspect(&unit[..unit.len().min(KEYFRAME_SCAN)]).idr
}

const KEYFRAME_SCAN: usize = 1024;

fn annexb_params(sps: &[u8], pps: &[u8], out: &mut Vec<u8>) {
    out.clear();
    for set in [sps, pps] {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(set);
    }
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    overrun: bool,
}

impl Bits<'_> {
    fn bit(&mut self) -> u32 {
        let Some(byte) = self.data.get(self.pos >> 3) else {
            self.overrun = true;
            return 0;
        };
        self.pos += 1;
        u32::from((byte >> (7 - ((self.pos - 1) & 7))) & 1)
    }

    fn bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            if self.overrun || zeros > 31 {
                self.overrun = true;
                return 0;
            }
            zeros += 1;
        }
        if zeros == 0 { 0 } else { (1u32 << zeros) - 1 + self.bits(zeros) }
    }

    fn se(&mut self) -> i32 {
        let k = self.ue();
        if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) }
    }
}

// Displayed picture size from a raw SPS (NAL header included, no start code).
pub(crate) fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    if sps.len() < 4 || sps[0] & 0x1F != 7 {
        return None;
    }
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &byte in &sps[1..] {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        rbsp.push(byte);
    }
    let mut r = Bits { data: &rbsp, pos: 0, overrun: false };
    let profile = r.bits(8);
    r.bits(16);
    r.ue();
    let mut chroma = 1;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = r.ue();
        if chroma == 3 {
            r.bit();
        }
        r.ue();
        r.ue();
        r.bit();
        if r.bit() == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit() == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se() + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue();
    match r.ue() {
        0 => {
            r.ue();
        }
        1 => {
            r.bit();
            r.se();
            r.se();
            for _ in 0..r.ue().min(255) {
                r.se();
            }
        }
        _ => {}
    }
    r.ue();
    r.bit();
    let width_mbs = r.ue() + 1;
    let height_units = r.ue() + 1;
    let frame_mbs_only = r.bit();
    if frame_mbs_only == 0 {
        r.bit();
    }
    r.bit();
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if r.bit() == 1 {
        left = r.ue();
        right = r.ue();
        top = r.ue();
        bottom = r.ue();
    }
    if r.overrun {
        return None;
    }
    let (crop_x, crop_y) = match chroma {
        0 | 3 => (1, 2 - frame_mbs_only),
        2 => (2, 2 - frame_mbs_only),
        _ => (2, 2 * (2 - frame_mbs_only)),
    };
    let width = (width_mbs * 16).checked_sub((left + right) * crop_x)?;
    let height = ((2 - frame_mbs_only) * height_units * 16).checked_sub((top + bottom) * crop_y)?;
    (width > 0 && height > 0).then_some((width, height))
}

// -------------------------------------------------------------------------------- GOP cache

// The access units since the last keyframe, so a recording that starts between keyframes can
// begin with a decodable picture. Bounded: when full, only the keyframe is kept.
pub struct GopCache {
    data: Vec<u8>,
    frames: Vec<(usize, usize, i64)>,
    params: Vec<u8>,
    cap: usize,
    full: bool,
}

impl GopCache {
    pub fn new(cap: usize) -> Self {
        Self { data: Vec::new(), frames: Vec::new(), params: Vec::new(), cap, full: false }
    }

    // `keyframe`: the unit starts a new group of pictures (an IDR, maybe with SPS/PPS).
    pub fn observe(&mut self, unit: &[u8], pts: i64, keyframe: bool) {
        if keyframe {
            self.data.clear();
            self.frames.clear();
            self.full = false;
            let found = inspect(unit);
            if let (Some(sps), Some(pps)) = (found.sps, found.pps) {
                annexb_params(sps, pps, &mut self.params);
            }
        } else if self.frames.is_empty() || self.full {
            return;
        }
        if !self.frames.is_empty() && self.data.len() + unit.len() > self.cap {
            self.full = true;
            let keep = self.frames[0].1;
            self.data.truncate(keep);
            self.frames.truncate(1);
            return;
        }
        let start = self.data.len();
        self.data.extend_from_slice(unit);
        self.frames.push((start, self.data.len(), pts));
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.frames.clear();
        self.full = false;
    }

    pub fn snapshot(&self) -> Option<Preroll> {
        (!self.frames.is_empty()).then(|| Preroll {
            data: self.data.clone(),
            frames: self.frames.clone(),
            params: self.params.clone(),
        })
    }
}

// ----------------------------------------------------------------------------- file naming

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: la cadena devuelta se copia y se libera con CoTaskMemFree.
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        path.map(PathBuf::from)
    }
}

fn home_folder(id: &GUID, fallback: &str) -> PathBuf {
    known_folder(id)
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(fallback)))
        .unwrap_or_else(|| PathBuf::from(fallback))
        .join("RustPlay")
}

// Videos\RustPlay.
pub fn default_recordings_dir() -> PathBuf {
    if let Some(dir) = crate::identity::portable_exe_dir() {
        return dir.join("Recordings");
    }
    home_folder(&FOLDERID_Videos, "Videos")
}

// Pictures\RustPlay.
pub fn default_screenshots_dir() -> PathBuf {
    if let Some(dir) = crate::identity::portable_exe_dir() {
        return dir.join("Screenshots");
    }
    home_folder(&FOLDERID_Pictures, "Pictures")
}

// "RustPlay 2026-11-15 14.03.22", from the local time.
pub fn timestamp_stem() -> String {
    // SAFETY: sin parámetros.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "RustPlay {:04}-{:02}-{:02} {:02}.{:02}.{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

// `folder\stem.ext`, or `folder\stem (n).ext` with the first free n.
pub fn unique_path(folder: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = folder.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..10_000)
        .map(|n| folder.join(format!("{stem} ({n}).{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

// --------------------------------------------------------------------------- writer thread

fn pack(a: u32, b: u32) -> u64 {
    (u64::from(a) << 32) | u64::from(b)
}

struct Writer {
    sink: IMFSinkWriter,
    video: u32,
    audio: Option<(u32, AudioSpec)>,
    path: PathBuf,
    sps: Vec<u8>,
    // Stream pts of timestamp 0.
    base: Option<i64>,
    last_ts: i64,
    // Wall time of the first live frame, and its timestamp in the file.
    clock: Option<(Instant, i64)>,
    audio_samples: u64,
    frames: u64,
}

fn video_type(sps: &[u8], params: &[u8]) -> Result<IMFMediaType> {
    let (w, h) = sps_dimensions(sps).ok_or_else(|| Error::Media("SPS de vídeo no válido".into()))?;
    // SAFETY: tipo de medio recién creado.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(60, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, 8_000_000)?;
        t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, params)?;
        Ok(t)
    }
}

fn aac_type(spec: AudioSpec) -> Result<IMFMediaType> {
    // SAFETY: tipo de medio recién creado.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, spec.sample_rate)?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(spec.channels))?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
        t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
        Ok(t)
    }
}

impl Writer {
    fn open(path: &Path, sps: &[u8], params: &[u8], audio: Option<AudioSpec>) -> Result<Self> {
        let video_t = video_type(sps, params)?;
        let wide = HSTRING::from(path.as_os_str());
        // SAFETY: objetos COM propios creados y usados en este hilo.
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 2)?;
            let attrs = attrs.ok_or_else(|| Error::Media("sin atributos".into()))?;
            attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let sink = MFCreateSinkWriterFromURL(&wide, None, &attrs)?;
            let video = sink.AddStream(&video_t)?;
            sink.SetInputMediaType(video, &video_t, None)?;
            let audio = match audio {
                Some(spec) => {
                    let index = sink.AddStream(&aac_type(spec)?)?;
                    sink.SetInputMediaType(index, &mf::pcm_type(spec.sample_rate, u32::from(spec.channels))?, None)?;
                    Some((index, spec))
                }
                None => None,
            };
            sink.BeginWriting()?;
            Ok(Self {
                sink,
                video,
                audio,
                path: path.to_owned(),
                sps: sps.to_vec(),
                base: None,
                last_ts: -1,
                clock: None,
                audio_samples: 0,
                frames: 0,
            })
        }
    }

    fn write(&self, stream: u32, bytes: &[u8], ts: i64, duration: i64, clean: bool) -> Result<()> {
        // SAFETY: búfer nuevo con capacidad suficiente, bloqueado durante la copia.
        unsafe {
            let buf = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            buf.Unlock()?;
            buf.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(ts)?;
            sample.SetSampleDuration(duration)?;
            if clean {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.sink.WriteSample(stream, &sample)?;
        }
        Ok(())
    }

    fn write_video(&mut self, unit: &[u8], ts: i64, idr: bool) -> Result<()> {
        let ts = ts.max(self.last_ts + 1);
        self.last_ts = ts;
        self.frames += 1;
        self.write(self.video, unit, ts, FRAME_DURATION, idr)
    }

    fn write_pcm(&mut self, pcm: &[i16]) -> Result<()> {
        let Some((index, spec)) = self.audio else { return Ok(()) };
        let channels = usize::from(spec.channels.max(1));
        let frames = (pcm.len() / channels) as u64;
        if frames == 0 {
            return Ok(());
        }
        let rate = i64::from(spec.sample_rate.max(1));
        let ts = self.audio_samples as i64 * TICKS / rate;
        let duration = frames as i64 * TICKS / rate;
        // SAFETY: reinterpretación de i16 como bytes (alineación 1, mismo tamaño total).
        let bytes = unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), frames as usize * channels * 2) };
        self.write(index, bytes, ts, duration, true)?;
        self.audio_samples += frames;
        Ok(())
    }

    // Pads with silence up to `until` (100 ns).
    fn silence_until(&mut self, until: i64, zeros: &[i16]) -> Result<()> {
        let Some((_, spec)) = self.audio else { return Ok(()) };
        let rate = i64::from(spec.sample_rate.max(1));
        let channels = usize::from(spec.channels.max(1));
        let mut missing = (until * rate / TICKS - self.audio_samples as i64).max(0) as usize;
        while missing > 0 {
            let frames = missing.min(zeros.len() / channels);
            self.write_pcm(&zeros[..frames * channels])?;
            missing -= frames;
        }
        Ok(())
    }

    // Timestamp of "now" on this file's timeline, once the first live frame fixed it.
    fn now_ts(&self, at: Instant) -> Option<i64> {
        let (wall, ts) = self.clock?;
        let elapsed = at.checked_duration_since(wall).unwrap_or_default();
        Some(ts + (elapsed.as_nanos() / 100) as i64)
    }

    fn finish(self, zeros: &[i16]) -> Result<PathBuf> {
        let mut this = self;
        if this.frames == 0 {
            drop(this.sink);
            let _ = std::fs::remove_file(&this.path);
            return Err(Error::Media("la grabación no contiene imágenes".into()));
        }
        if this.audio.is_some() && this.audio_samples == 0 {
            // An audio track without samples confuses some players: fill it with silence.
            let end = this.last_ts + FRAME_DURATION;
            this.silence_until(end, zeros)?;
        }
        // SAFETY: cierre del escritor propio.
        unsafe { this.sink.Finalize()? };
        Ok(this.path)
    }
}

struct Session<'a> {
    config: RecorderConfig,
    notify: &'a Notify,
    writer: Option<Writer>,
    segment: u32,
    params: Vec<u8>,
    zeros: Vec<i16>,
    failed: bool,
    waiting_logged: bool,
}

impl Session<'_> {
    fn fail(&mut self, error: &Error) {
        tracing::warn!("grabación interrumpida: {error}");
        (self.notify)(RecorderEvent::Failed(error.to_string()));
        self.failed = true;
        if let Some(writer) = self.writer.take() {
            self.close(writer);
        }
    }

    fn close(&mut self, writer: Writer) {
        match writer.finish(&self.zeros) {
            Ok(path) => {
                tracing::info!("grabación guardada en {}", path.display());
                (self.notify)(RecorderEvent::Saved(path));
            }
            Err(e) => {
                tracing::warn!("no se pudo cerrar la grabación: {e}");
                (self.notify)(RecorderEvent::Failed(e.to_string()));
            }
        }
    }

    fn open(&mut self, sps: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.config.folder)?;
        self.segment += 1;
        let stem = if self.segment == 1 {
            self.config.stem.clone()
        } else {
            format!("{} ({})", self.config.stem, self.segment)
        };
        let path = unique_path(&self.config.folder, &stem, "mp4");
        let writer = match Writer::open(&path, sps, &self.params, self.config.audio) {
            Ok(writer) => writer,
            Err(e) if self.config.audio.is_some() => {
                tracing::warn!("no se pudo preparar el audio AAC de la grabación ({e}); se graba sin sonido");
                let _ = std::fs::remove_file(&path);
                self.config.audio = None;
                Writer::open(&path, sps, &self.params, None)?
            }
            Err(e) => return Err(e),
        };
        tracing::info!("grabando la duplicación en {}", path.display());
        (self.notify)(RecorderEvent::Started(path));
        self.writer = Some(writer);
        Ok(())
    }

    // Makes sure a file is open for this unit; false if the unit must be skipped.
    fn prepare(&mut self, unit: &[u8]) -> Result<Option<bool>> {
        let found = inspect(unit);
        if let (Some(sps), Some(pps)) = (found.sps, found.pps) {
            let changed = self.writer.as_ref().is_some_and(|w| w.sps != sps);
            annexb_params(sps, pps, &mut self.params);
            if changed {
                tracing::info!("el formato del vídeo cambió: la grabación sigue en un archivo nuevo");
                if let Some(writer) = self.writer.take() {
                    self.close(writer);
                }
            }
        }
        if self.writer.is_none() {
            if !found.idr {
                if !self.waiting_logged {
                    self.waiting_logged = true;
                    tracing::debug!("grabación: esperando un fotograma clave");
                }
                return Ok(None);
            }
            let sps = match found.sps {
                Some(sps) => sps.to_vec(),
                None => match nal_units(&self.params).find(|n| n[0] & 0x1F == 7) {
                    Some(sps) => sps.to_vec(),
                    None => return Ok(None),
                },
            };
            self.open(&sps)?;
        }
        Ok(Some(found.idr))
    }

    fn preroll(&mut self, preroll: &Preroll) -> Result<usize> {
        if !preroll.params.is_empty() {
            self.params.clone_from(&preroll.params);
        }
        let mut written = 0;
        for &(start, end, _) in &preroll.frames {
            let unit = &preroll.data[start..end];
            let Some(idr) = self.prepare(unit)? else { continue };
            if let Some(writer) = self.writer.as_mut() {
                writer.write_video(unit, written as i64 * PREROLL_STEP, idr)?;
                written += 1;
            }
        }
        if let Some(writer) = self.writer.as_mut().filter(|_| written > 0) {
            writer.clock = Some((Instant::now(), writer.last_ts + PREROLL_STEP));
            tracing::debug!(frames = written, "grabación: se incluyen las imágenes desde el último fotograma clave");
        }
        Ok(written)
    }

    fn video(&mut self, unit: &[u8], pts: i64) -> Result<()> {
        let Some(idr) = self.prepare(unit)? else { return Ok(()) };
        let Some(writer) = self.writer.as_mut() else { return Ok(()) };
        let base = match writer.base {
            Some(base) => base,
            None => {
                // The first live frame lands where the wall clock says (after a preroll, the
                // time since recording started); later frames follow the stream's own clock.
                let now = Instant::now();
                let ts = writer.now_ts(now).unwrap_or(0).max(writer.last_ts + 1);
                if writer.clock.is_none() {
                    writer.clock = Some((now, ts));
                }
                *writer.base.insert(pts - ts)
            }
        };
        writer.write_video(unit, (pts - base).max(0), idr)
    }

    fn audio(&mut self, pcm: &[i16], at: Instant) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else { return Ok(()) };
        let Some((_, spec)) = writer.audio else { return Ok(()) };
        let Some(now) = writer.now_ts(at) else { return Ok(()) };
        let rate = i64::from(spec.sample_rate.max(1));
        let written = writer.audio_samples as i64 * TICKS / rate;
        if now - written > AUDIO_GAP {
            writer.silence_until(now, &self.zeros)?;
        }
        writer.write_pcm(pcm)
    }
}

fn run(config: RecorderConfig, rx: &Receiver<Msg>, shared: &Shared, notify: &Notify) {
    if let Err(e) = mf::ensure_started() {
        (notify)(RecorderEvent::Failed(e.to_string()));
        return;
    }
    let zeros = vec![0i16; config.audio.map_or(0, |a| a.sample_rate as usize * usize::from(a.channels))];
    let mut session = Session {
        config,
        notify,
        writer: None,
        segment: 0,
        params: Vec::new(),
        zeros,
        failed: false,
        waiting_logged: false,
    };
    if let Some(preroll) = session.config.preroll.take()
        && let Err(e) = session.preroll(&preroll)
    {
        session.fail(&e);
    }
    loop {
        let msg = match rx.recv_timeout(IDLE_WAIT) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) if shared.stop.load(Ordering::Acquire) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            Msg::Video(buf, pts) => {
                if !session.failed
                    && let Err(e) = session.video(&buf, pts)
                {
                    session.fail(&e);
                }
                recycle_video(shared, buf);
            }
            Msg::Audio(buf, at) => {
                if !session.failed
                    && let Err(e) = session.audio(&buf, at)
                {
                    session.fail(&e);
                }
                recycle_audio(shared, buf);
            }
        }
    }
    if let Some(writer) = session.writer.take() {
        session.close(writer);
    } else if !session.failed {
        (notify)(RecorderEvent::Failed("no llegó ninguna imagen que grabar".into()));
    }
    let (video, audio) = (shared.dropped_video.load(Ordering::Relaxed), shared.dropped_audio.load(Ordering::Relaxed));
    if video + audio > 0 {
        tracing::debug!(video, audio, "grabación: datos descartados por cola llena");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    use super::*;
    use crate::media::gpu::GpuDevice;
    use crate::media::video::{EncoderConfig, H264Encoder};

    #[test]
    fn nal_units_split_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
        let found = inspect(&data);
        assert!(found.idr);
        assert_eq!(found.sps, Some(&[0x67, 1, 2][..]));
        assert_eq!(found.pps, Some(&[0x68, 3][..]));
    }

    #[test]
    fn sps_dimensions_read_cropping() {
        // 1280x720 Baseline (the vector the sender's tests use).
        assert_eq!(sps_dimensions(&[0x67, 0x42, 0x00, 0x1F, 0xF8, 0x0A, 0x00, 0xB7, 0x20]), Some((1280, 720)));
        assert_eq!(sps_dimensions(&[0x68, 1, 2, 3]), None);
    }

    #[test]
    fn gop_cache_keeps_units_since_the_last_keyframe() {
        let mut gop = GopCache::new(64);
        gop.observe(&[0, 0, 1, 0x41, 1], 1, false);
        assert!(gop.snapshot().is_none(), "nothing before the first keyframe");
        gop.observe(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3], 2, true);
        gop.observe(&[0, 0, 1, 0x41, 4], 3, false);
        let snap = gop.snapshot().unwrap();
        assert_eq!(snap.frames(), 2);
        assert_eq!(snap.params, [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 2]);
        for _ in 0..20 {
            gop.observe(&[0, 0, 1, 0x41, 5, 6, 7], 4, false);
        }
        assert_eq!(gop.snapshot().unwrap().frames(), 1, "when full only the keyframe stays");
        gop.observe(&[0, 0, 1, 0x65, 9], 5, true);
        let snap = gop.snapshot().unwrap();
        assert_eq!(snap.frames(), 1);
        assert_eq!(snap.params.len(), 12, "parameter sets survive a keyframe without them");
    }

    #[test]
    fn unique_paths_add_a_counter() {
        let dir = std::env::temp_dir().join(format!("rustplay-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_path(&dir, "a", "mp4");
        assert_eq!(first, dir.join("a.mp4"));
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(unique_path(&dir, "a", "mp4"), dir.join("a (2).mp4"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(timestamp_stem().starts_with("RustPlay 20"));
    }

    fn nv12_texture(gpu: &GpuDevice, w: u32, h: u32, frame: u32) -> Option<ID3D11Texture2D> {
        let mut pixels = vec![0u8; (w * h * 3 / 2) as usize];
        for y in 0..h {
            for x in 0..w {
                pixels[(y * w + x) as usize] = ((x + y + frame * 8) % 220 + 16) as u8;
            }
        }
        for (i, c) in pixels[(w * h) as usize..].iter_mut().enumerate() {
            *c = if i % 2 == 0 { 100 + (frame % 50) as u8 } else { 150 };
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: w, SysMemSlicePitch: 0 };
        let mut tex = None;
        // SAFETY: datos iniciales válidos durante la llamada.
        unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)).ok()? };
        tex
    }

    #[test]
    fn records_a_short_mp4_from_synthetic_h264() {
        let Ok(gpu) = GpuDevice::create() else {
            eprintln!("sin GPU: se omite la prueba de grabación");
            return;
        };
        let (w, h) = (320, 240);
        let config = EncoderConfig { width: w, height: h, fps: 30, bitrate: 1_000_000 };
        let mut encoder = match H264Encoder::new(Arc::clone(&gpu), config) {
            Ok(encoder) => encoder,
            Err(e) => {
                eprintln!("sin codificador H.264 ({e}): se omite la prueba de grabación");
                return;
            }
        };
        let textures: Vec<_> = (0..4).filter_map(|i| nv12_texture(&gpu, w, h, i)).collect();
        if textures.len() != 4 {
            eprintln!("no se pudieron crear texturas NV12: se omite la prueba");
            return;
        }
        let mut units = Vec::new();
        for i in 0..45u32 {
            let pts = i64::from(i) * TICKS / 30;
            if let Some(frame) = encoder.encode(&textures[(i % 4) as usize], pts).expect("codificar") {
                units.push((frame.data.to_vec(), frame.pts));
            }
            while let Some(frame) = encoder.poll_output().expect("salida") {
                units.push((frame.data.to_vec(), frame.pts));
            }
        }
        assert!(units.len() >= 20, "pocas unidades: {}", units.len());

        let dir = std::env::temp_dir().join(format!("rustplay-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let recorder = Recorder::start(
            RecorderConfig {
                folder: dir.clone(),
                stem: "prueba".into(),
                audio: Some(AudioSpec { sample_rate: 44_100, channels: 2 }),
                preroll: None,
            },
            move |e| sink.lock().unwrap().push(e),
        )
        .expect("grabador");
        let tone: Vec<i16> = (0..352 * 2).map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i16).collect();
        for (unit, pts) in &units {
            while !recorder.push_video(unit, *pts + 123_456_789) {
                std::thread::sleep(Duration::from_millis(2));
            }
            for _ in 0..4 {
                recorder.push_audio(&tone);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        recorder.finish();
        let events = events.lock().unwrap().clone();
        let saved: Vec<&PathBuf> = events
            .iter()
            .filter_map(|e| match e {
                RecorderEvent::Saved(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(saved.len(), 1, "eventos: {events:?}");
        let bytes = std::fs::read(saved[0]).expect("archivo MP4");
        assert!(bytes.len() > 4_000, "MP4 demasiado pequeño: {} bytes", bytes.len());
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|w| w == b"moov"));
        assert!(bytes.windows(4).any(|w| w == b"avcC"));
        assert!(bytes.windows(4).any(|w| w == b"mp4a"), "falta la pista AAC");
        // Lets the video player test reuse this clip.
        if let Some(keep) = std::env::var_os("RUSTPLAY_KEEP_MP4") {
            let _ = std::fs::copy(saved[0], keep);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_sps_starts_a_new_file() {
        if mf::ensure_started().is_err() {
            return;
        }
        let Ok(gpu) = GpuDevice::create() else {
            eprintln!("sin GPU: se omite la prueba");
            return;
        };
        let dir = std::env::temp_dir().join(format!("rustplay-rec-seg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let recorder = Recorder::start(
            RecorderConfig { folder: dir.clone(), stem: "giro".into(), audio: None, preroll: None },
            move |e| sink.lock().unwrap().push(e),
        )
        .expect("grabador");
        let mut pts = 0;
        for (w, h) in [(320, 240), (240, 320)] {
            let Ok(mut encoder) = H264Encoder::new(Arc::clone(&gpu), EncoderConfig { width: w, height: h, fps: 30, bitrate: 800_000 }) else {
                eprintln!("sin codificador H.264: se omite la prueba");
                return;
            };
            let Some(tex) = nv12_texture(&gpu, w, h, 0) else { return };
            for _ in 0..20 {
                pts += TICKS / 30;
                if let Some(frame) = encoder.encode(&tex, pts).expect("codificar") {
                    while !recorder.push_video(frame.data, frame.pts) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
        recorder.finish();
        let events = events.lock().unwrap().clone();
        let saved: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                RecorderEvent::Saved(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(saved, vec!["giro.mp4".to_owned(), "giro (2).mp4".to_owned()], "eventos: {events:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
