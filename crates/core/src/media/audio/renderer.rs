use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};
use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, WaitForMultipleObjects,
};
use windows::core::w;

use super::devices::active_device;
use super::wasapi::{Event, default_device, format_44k, is_invalidated};
use super::{CHANNELS, SAMPLE_RATE};
use crate::media::mf::ensure_com;
use crate::{Error, Result};

const STREAM_FLAGS: u32 = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
const FALLBACK_BUFFER_100NS: i64 = 200_000;
// Room for the largest playout delay a receiver may add on top of its latency.
pub const MAX_EXTRA_DELAY_MS: u32 = 500;

struct Shared {
    stop: AtomicBool,
    stop_event: Event,
    volume: AtomicU32,
    flush_seq: AtomicU32,
    flush_target: AtomicU64,
    popped: AtomicU64,
    padding: AtomicU32,
    latency_100ns: AtomicU64,
    period_frames: AtomicU32,
    buffer_frames: AtomicU32,
}

struct Stream {
    client: IAudioClient3,
    render: IAudioRenderClient,
    event: Event,
    buffer_frames: u32,
}

pub struct Renderer {
    producer: Producer<i16>,
    shared: Arc<Shared>,
    pushed: u64,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    pub fn start(latency_ms: u32) -> Result<Self> {
        Self::start_on(latency_ms, None)
    }

    // Plays on the endpoint with this ID (see `render_devices`), or on the default one when
    // it is `None` or no longer available; a device that disappears later also falls back.
    pub fn start_on(latency_ms: u32, device: Option<&str>) -> Result<Self> {
        let ring_samples = ((latency_ms + 200 + MAX_EXTRA_DELAY_MS) as usize * SAMPLE_RATE as usize
            / 1000)
            * CHANNELS as usize;
        let (producer, consumer) = RingBuffer::<i16>::new(ring_samples);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            stop_event: Event::new()?,
            volume: AtomicU32::new(1.0f32.to_bits()),
            flush_seq: AtomicU32::new(0),
            flush_target: AtomicU64::new(0),
            popped: AtomicU64::new(0),
            padding: AtomicU32::new(0),
            latency_100ns: AtomicU64::new(0),
            period_frames: AtomicU32::new(0),
            buffer_frames: AtomicU32::new(0),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let sh = shared.clone();
        let device = device.map(str::to_owned);
        let thread = std::thread::Builder::new()
            .name("rustplay-audio-render".into())
            .spawn(move || render_main(sh, consumer, tx, device))
            .map_err(Error::Io)?;
        match rx.recv() {
            Ok(Ok(())) => Ok(Self {
                producer,
                shared,
                pushed: 0,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(Error::Media(
                "el hilo de audio terminó inesperadamente".into(),
            )),
        }
    }

    pub fn push(&mut self, pcm: &[i16]) -> usize {
        let n = (pcm.len() & !1).min(self.producer.slots());
        if n == 0 {
            return 0;
        }
        if let Ok(mut chunk) = self.producer.write_chunk(n) {
            let (a, b) = chunk.as_mut_slices();
            a.copy_from_slice(&pcm[..a.len()]);
            b.copy_from_slice(&pcm[a.len()..n]);
            chunk.commit_all();
        }
        self.pushed += n as u64;
        n / 2
    }

    pub fn set_volume(&mut self, linear: f32) {
        let v = if linear.is_nan() {
            0.0
        } else {
            linear.clamp(0.0, 1.0)
        };
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn flush(&mut self) {
        self.shared
            .flush_target
            .store(self.pushed, Ordering::Release);
        self.shared.flush_seq.fetch_add(1, Ordering::Release);
    }

    pub fn buffered_frames(&self) -> usize {
        let queued = self
            .pushed
            .saturating_sub(self.shared.popped.load(Ordering::Acquire));
        queued as usize / 2 + self.shared.padding.load(Ordering::Relaxed) as usize
    }

    pub fn free_frames(&self) -> usize {
        self.producer.slots() / 2
    }

    pub fn device_latency(&self) -> Duration {
        Duration::from_nanos(self.shared.latency_100ns.load(Ordering::Relaxed) * 100)
    }

    pub fn period_frames(&self) -> u32 {
        self.shared.period_frames.load(Ordering::Relaxed)
    }

    pub fn buffer_frames(&self) -> u32 {
        self.shared.buffer_frames.load(Ordering::Relaxed)
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.stop_event.set();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pick_device(wanted: Option<&str>) -> Result<IMMDevice> {
    if let Some(id) = wanted {
        if let Some(device) = active_device(id) {
            return Ok(device);
        }
        tracing::warn!("el altavoz elegido no está disponible; se usa el predeterminado");
    }
    default_device(eRender)
}

fn open_stream(shared: &Shared, wanted: Option<&str>) -> Result<Stream> {
    let device = pick_device(wanted)?;
    let fmt = format_44k();
    // SAFETY: llamadas COM sobre objetos propios; `fmt` vive durante las llamadas.
    unsafe {
        let mut client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
        let mut period = 0u32;
        let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        let low_latency = client
            .GetSharedModeEnginePeriod(&fmt, &mut def, &mut fund, &mut min, &mut max)
            .and_then(|()| client.InitializeSharedAudioStream(STREAM_FLAGS, min, &fmt, None))
            .is_ok();
        if low_latency {
            period = min;
        } else {
            client = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                STREAM_FLAGS,
                FALLBACK_BUFFER_100NS,
                0,
                &fmt,
                None,
            )?;
        }
        let event = Event::new()?;
        client.SetEventHandle(event.0)?;
        let buffer_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        let stream_latency = client.GetStreamLatency().unwrap_or(0).max(0) as u64;
        let buf_100ns = buffer_frames as u64 * 10_000_000 / SAMPLE_RATE as u64;
        shared
            .latency_100ns
            .store(stream_latency + buf_100ns, Ordering::Relaxed);
        shared.period_frames.store(period, Ordering::Relaxed);
        shared.buffer_frames.store(buffer_frames, Ordering::Relaxed);
        Ok(Stream {
            client,
            render,
            event,
            buffer_frames,
        })
    }
}

fn render_main(
    shared: Arc<Shared>,
    mut consumer: Consumer<i16>,
    ready: std::sync::mpsc::Sender<Result<()>>,
    device: Option<String>,
) {
    if let Err(e) = ensure_com() {
        let _ = ready.send(Err(e));
        return;
    }
    let mut task = 0u32;
    // SAFETY: registro MMCSS del hilo actual; se revierte al salir.
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) }.ok();
    let mut stream = match open_stream(&shared, device.as_deref()) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut popped = 0u64;
    let mut seen_flush = 0u32;
    while !shared.stop.load(Ordering::Acquire) {
        match run_stream(
            &stream,
            &shared,
            &mut consumer,
            &mut popped,
            &mut seen_flush,
        ) {
            Ok(()) => break,
            Err(e) if is_invalidated(&e) => {
                tracing::warn!("dispositivo de audio invalidado; reabriendo");
                stream = loop {
                    if shared.stop.load(Ordering::Acquire) {
                        break stream;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    match open_stream(&shared, device.as_deref()) {
                        Ok(s) => break s,
                        Err(e) => tracing::debug!("reapertura de audio fallida: {e}"),
                    }
                };
            }
            Err(e) => {
                tracing::error!("fallo del renderizador de audio: {e}");
                break;
            }
        }
    }
    // SAFETY: el cliente sigue vivo; se ignoran los errores al detener.
    unsafe {
        let _ = stream.client.Stop();
        if let Some(h) = mmcss {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
}

fn run_stream(
    s: &Stream,
    shared: &Shared,
    consumer: &mut Consumer<i16>,
    popped: &mut u64,
    seen_flush: &mut u32,
) -> windows::core::Result<()> {
    // SAFETY: el búfer devuelto por GetBuffer es válido para `frames * 4` bytes
    // hasta el ReleaseBuffer correspondiente.
    unsafe {
        let silent = |frames: u32| -> windows::core::Result<()> {
            s.render.GetBuffer(frames)?;
            s.render
                .ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        };
        silent(s.buffer_frames)?;
        s.client.Start()?;
        let handles = [s.event.0, shared.stop_event.0];
        loop {
            let r: WAIT_EVENT = WaitForMultipleObjects(&handles, false, 2000);
            if r != WAIT_OBJECT_0 {
                if r.0 == WAIT_OBJECT_0.0 + 1 || shared.stop.load(Ordering::Acquire) {
                    s.client.Stop()?;
                    return Ok(());
                }
                continue;
            }
            let seq = shared.flush_seq.load(Ordering::Acquire);
            if seq != *seen_flush {
                *seen_flush = seq;
                let target = shared.flush_target.load(Ordering::Acquire);
                let need = (target.saturating_sub(*popped) as usize).min(consumer.slots());
                if need > 0
                    && let Ok(c) = consumer.read_chunk(need)
                {
                    c.commit_all();
                    *popped += need as u64;
                    shared.popped.store(*popped, Ordering::Release);
                }
                s.client.Stop()?;
                s.client.Reset()?;
                silent(s.buffer_frames)?;
                s.client.Start()?;
                continue;
            }
            let padding = s.client.GetCurrentPadding()?;
            let frames = s.buffer_frames.saturating_sub(padding);
            if frames == 0 {
                continue;
            }
            let p = s.render.GetBuffer(frames)?;
            let out = std::slice::from_raw_parts_mut(p as *mut i16, frames as usize * 2);
            let n = fill(
                out,
                consumer,
                f32::from_bits(shared.volume.load(Ordering::Relaxed)),
            );
            if n > 0 {
                *popped += n as u64;
                shared.popped.store(*popped, Ordering::Release);
            }
            shared.padding.store(padding + frames, Ordering::Relaxed);
            s.render.ReleaseBuffer(frames, 0)?;
        }
    }
}

fn fill(out: &mut [i16], consumer: &mut Consumer<i16>, gain: f32) -> usize {
    let n = consumer.slots().min(out.len());
    if n > 0
        && let Ok(chunk) = consumer.read_chunk(n)
    {
        let (a, b) = chunk.as_slices();
        out[..a.len()].copy_from_slice(a);
        out[a.len()..n].copy_from_slice(b);
        chunk.commit_all();
        if gain < 1.0 {
            for s in &mut out[..n] {
                *s = (*s as f32 * gain) as i16;
            }
        }
    }
    out[n..].fill(0);
    n
}
