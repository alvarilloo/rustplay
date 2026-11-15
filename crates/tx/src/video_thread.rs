use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustplay_core::media::capture::{CaptureSource, Capturer};
use rustplay_core::media::gpu::GpuDevice;
use rustplay_core::media::video::{EncoderConfig, H264Encoder};
use rustplay_core::{Error, Result};
use tokio::sync::{mpsc, watch};

use crate::video::{MirrorStats, Unit, UnitBuilder};

const READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub struct VideoParams {
    pub monitor: u32,
    // When set, shares this window (HWND) instead of the monitor.
    pub window: Option<isize>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub cursor: bool,
}

// State shared by the video thread and the network tasks.
pub struct Shared {
    pub stop: AtomicBool,
    pub want_keyframe: AtomicBool,
    pub stats: MirrorStats,
    failure: Mutex<Option<String>>,
    closed: watch::Sender<bool>,
}

impl Shared {
    pub fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (closed, closed_rx) = watch::channel(false);
        let shared = Arc::new(Self {
            stop: AtomicBool::new(false),
            want_keyframe: AtomicBool::new(false),
            stats: MirrorStats::default(),
            failure: Mutex::new(None),
            closed,
        });
        (shared, closed_rx)
    }

    pub fn fail(&self, reason: impl Into<String>) {
        let mut slot = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(reason.into());
        }
        drop(slot);
        let _ = self.closed.send(true);
    }

    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

pub struct VideoThread {
    go: Option<std_mpsc::Sender<()>>,
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    pub encoder: String,
}

impl VideoThread {
    // Creates the GPU device, duplicator and encoder on a dedicated thread (COM MTA) and waits until
    // they are ready, so capture problems surface before the receiver is asked to start.
    pub fn spawn(
        params: VideoParams,
        shared: Arc<Shared>,
        pool: mpsc::Receiver<Vec<u8>>,
        units: mpsc::Sender<Unit>,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<String>>();
        let (go_tx, go_rx) = std_mpsc::channel::<()>();
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("rustplay-video".into())
            .spawn(move || run(params, &thread_shared, pool, units, &ready_tx, &go_rx))
            .map_err(Error::Io)?;
        let mut thread = Self { go: Some(go_tx), shared, handle: Some(handle), encoder: String::new() };
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(name)) => {
                thread.encoder = name;
                Ok(thread)
            }
            Ok(Err(e)) => {
                thread.shutdown();
                Err(e)
            }
            Err(_) => {
                thread.shutdown();
                Err(Error::Timeout)
            }
        }
    }

    #[cfg(test)]
    pub fn inert(shared: Arc<Shared>) -> Self {
        Self { go: None, shared, handle: None, encoder: String::from("prueba") }
    }

    pub fn start(&mut self) {
        if let Some(go) = self.go.take() {
            let _ = go.send(());
        }
    }

    pub fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.go = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VideoThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    params: VideoParams,
    shared: &Shared,
    pool: mpsc::Receiver<Vec<u8>>,
    units: mpsc::Sender<Unit>,
    ready: &std_mpsc::Sender<Result<String>>,
    go: &std_mpsc::Receiver<()>,
) {
    let built = (|| -> Result<(Capturer, H264Encoder)> {
        let gpu = GpuDevice::create()?;
        let source = params.window.map_or(CaptureSource::Monitor(params.monitor), CaptureSource::Window);
        let mut dup = Capturer::new(Arc::clone(&gpu), source, params.width, params.height)?;
        dup.set_max_fps(params.fps);
        dup.set_draw_cursor(params.cursor);
        let encoder = H264Encoder::new(
            gpu,
            EncoderConfig { width: params.width, height: params.height, fps: params.fps, bitrate: params.bitrate },
        )?;
        Ok((dup, encoder))
    })();
    let (mut dup, mut encoder) = match built {
        Ok(pair) => pair,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let name = format!("{}{}", encoder.name(), if encoder.is_hardware() { " (hardware)" } else { " (software)" });
    let _ = ready.send(Ok(name));
    if go.recv().is_err() {
        return;
    }
    if let Err(e) = capture_loop(params, shared, &mut dup, &mut encoder, pool, units) {
        if !shared.stopping() {
            shared.fail(format!("error en la captura o la codificación de vídeo: {e}"));
        }
    }
}

// Converts encoder output into pooled units and applies the drop policy: once anything is lost
// the stream is held until the next IDR so the receiver never decodes a broken reference chain.
pub struct Feeder {
    builder: UnitBuilder,
    pool: mpsc::Receiver<Vec<u8>>,
    units: mpsc::Sender<Unit>,
    spare: Option<Vec<u8>>,
    need_idr: bool,
    origin: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fed {
    Queued,
    Skipped,
    Lost,
}

impl Feeder {
    pub fn new(
        size: (u32, u32),
        pool: mpsc::Receiver<Vec<u8>>,
        units: mpsc::Sender<Unit>,
        origin: Instant,
    ) -> Self {
        Self { builder: UnitBuilder::new(size), pool, units, spare: None, need_idr: true, origin }
    }

    pub fn require_idr(&mut self) {
        self.need_idr = true;
    }

    pub fn seed(&mut self, sps: &[u8], pps: &[u8]) {
        self.builder.seed(sps, pps);
    }

    pub fn capture_time(&self, pts: i64) -> Instant {
        self.origin + Duration::from_nanos(pts.max(0) as u64 * 100)
    }

    // `Lost` means a keyframe must be requested from the encoder.
    pub fn feed(&mut self, annexb: &[u8], pts: i64) -> Fed {
        let Some(mut buf) = self.spare.take().or_else(|| self.pool.try_recv().ok()) else {
            self.need_idr = true;
            return Fed::Lost;
        };
        let Some(built) = self.builder.build(annexb, &mut buf) else {
            self.spare = Some(buf);
            return Fed::Skipped;
        };
        if self.need_idr && !built.keyframe {
            self.spare = Some(buf);
            return Fed::Skipped;
        }
        let had_codec = built.codec.is_some();
        let unit = Unit { buf, capture: self.capture_time(pts), keyframe: built.keyframe, codec: built.codec };
        match self.units.try_send(unit) {
            Ok(()) => {
                if built.keyframe {
                    self.need_idr = false;
                }
                Fed::Queued
            }
            Err(mpsc::error::TrySendError::Full(unit) | mpsc::error::TrySendError::Closed(unit)) => {
                self.spare = Some(unit.buf);
                if had_codec {
                    self.builder.forget_sent();
                }
                self.need_idr = true;
                Fed::Lost
            }
        }
    }
}

fn capture_loop(
    params: VideoParams,
    shared: &Shared,
    dup: &mut Capturer,
    encoder: &mut H264Encoder,
    pool: mpsc::Receiver<Vec<u8>>,
    units: mpsc::Sender<Unit>,
) -> Result<()> {
    let interval = Duration::from_micros(1_000_000 / u64::from(params.fps.max(1)));
    let origin = Instant::now();
    let mut feeder = Feeder::new((params.width, params.height), pool, units, origin);
    let mut last_pts = 0i64;
    let mut seeded = false;
    encoder.request_keyframe();
    while !shared.stopping() {
        if shared.want_keyframe.swap(false, Ordering::AcqRel) {
            feeder.require_idr();
            encoder.request_keyframe();
        }
        let texture = match dup.next_frame(interval)? {
            Some(frame) => {
                MirrorStats::add(&shared.stats.captured, 1);
                frame.nv12.clone()
            }
            None => match dup.last_frame() {
                Some(frame) => {
                    MirrorStats::add(&shared.stats.repeated, 1);
                    frame.nv12.clone()
                }
                None => continue,
            },
        };
        let captured_at = Instant::now();
        let pts = ((captured_at.saturating_duration_since(origin).as_nanos() / 100) as i64).max(last_pts + 1);
        last_pts = pts;
        let started = Instant::now();
        let mut fed = Vec::new();
        let output = encoder.encode(&texture, pts)?;
        let micros = started.elapsed().as_micros() as u64;
        MirrorStats::add(&shared.stats.encode_micros, micros);
        shared.stats.encode_max_micros.fetch_max(micros, Ordering::Relaxed);
        if let Some(frame) = output {
            MirrorStats::add(&shared.stats.encoded, 1);
            fed.push(feeder.feed(frame.data, frame.pts));
        }
        while let Some(frame) = encoder.poll_output()? {
            MirrorStats::add(&shared.stats.encoded, 1);
            fed.push(feeder.feed(frame.data, frame.pts));
        }
        for outcome in fed {
            match outcome {
                Fed::Queued => {}
                Fed::Skipped => MirrorStats::add(&shared.stats.dropped, 1),
                Fed::Lost => {
                    MirrorStats::add(&shared.stats.dropped, 1);
                    encoder.request_keyframe();
                }
            }
        }
        if !seeded && let Some((sps, pps)) = encoder.sps_pps() {
            feeder.seed(sps, pps);
            seeded = true;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 9] = [0x67, 0x42, 0x00, 0x1F, 0xF8, 0x0A, 0x00, 0xB7, 0x20];
    const PPS: [u8; 4] = [0x68, 0xCE, 0x06, 0xE2];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], n].concat()).collect()
    }

    fn idr() -> Vec<u8> {
        annexb(&[&SPS, &PPS, &[0x65, 0x88, 0x84]])
    }

    fn p_frame() -> Vec<u8> {
        annexb(&[&[0x41, 0x9A, 0x01]])
    }

    fn feeder(pool_size: usize, queue: usize) -> (Feeder, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Unit>) {
        let (pool_tx, pool_rx) = mpsc::channel(8);
        for _ in 0..pool_size {
            pool_tx.try_send(Vec::new()).unwrap();
        }
        let (units_tx, units_rx) = mpsc::channel(queue);
        (Feeder::new((1280, 720), pool_rx, units_tx, Instant::now()), pool_tx, units_rx)
    }

    #[test]
    fn frames_wait_for_the_first_idr() {
        let (mut feeder, _pool, mut rx) = feeder(4, 4);
        assert_eq!(feeder.feed(&p_frame(), 1), Fed::Skipped);
        assert_eq!(feeder.feed(&idr(), 2), Fed::Queued);
        assert_eq!(feeder.feed(&p_frame(), 3), Fed::Queued);
        let first = rx.try_recv().unwrap();
        assert!(first.keyframe && first.codec.is_some());
        assert!(!rx.try_recv().unwrap().keyframe);
    }

    #[test]
    fn a_full_queue_drops_until_the_next_idr_and_resends_the_codec() {
        let (mut feeder, pool, mut rx) = feeder(8, 1);
        assert_eq!(feeder.feed(&idr(), 1), Fed::Queued);
        assert_eq!(feeder.feed(&p_frame(), 2), Fed::Lost, "queue full");
        let first = rx.try_recv().unwrap();
        pool.try_send(first.buf).unwrap();
        assert_eq!(feeder.feed(&p_frame(), 3), Fed::Skipped, "held until a keyframe");
        assert_eq!(feeder.feed(&idr(), 4), Fed::Queued);
        let again = rx.try_recv().unwrap();
        assert!(again.keyframe);
        assert!(again.codec.is_none(), "the configuration already went out with the first IDR");
    }

    #[test]
    fn a_lost_idr_re_advertises_the_codec() {
        let (mut feeder, _pool, mut rx) = feeder(8, 1);
        assert_eq!(feeder.feed(&idr(), 1), Fed::Queued);
        assert_eq!(feeder.feed(&idr(), 2), Fed::Lost);
        let _ = rx.try_recv().unwrap();
        feeder.require_idr();
        assert_eq!(feeder.feed(&idr(), 3), Fed::Queued);
        assert!(rx.try_recv().unwrap().keyframe);
    }

    #[test]
    fn an_empty_pool_loses_the_frame() {
        let (mut feeder, _pool, _rx) = feeder(0, 4);
        assert_eq!(feeder.feed(&idr(), 1), Fed::Lost);
    }

    #[test]
    fn capture_time_comes_from_the_pts() {
        let origin = Instant::now();
        let (_, pool_rx) = mpsc::channel(1);
        let (units_tx, _units_rx) = mpsc::channel(1);
        let feeder = Feeder::new((16, 16), pool_rx, units_tx, origin);
        assert_eq!(feeder.capture_time(10_000_000), origin + Duration::from_secs(1));
    }
}
