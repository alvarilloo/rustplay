use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use rustplay_core::media::audio::{AlacEncoder, LoopbackCapture};
use rustplay_core::ntp::NtpTimestamp;
use rustplay_core::{Error, Result};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle as TaskHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use crate::conn::RtspConn;
use crate::clock;
use crate::pacing::{self, Action, Pacer};
use crate::rtp::{self, PacketBuilder, PacketRing, SLOT_SIZE, SPF, SYNC_LEN};

const POOL_FRAMES: usize = 16;
const FRAME_CAPACITY: usize = 2048;
const PCM_SAMPLES: usize = SPF as usize * 2;
const PRE_ROLL: Duration = Duration::from_millis(40);
const RESTART_DELAY: Duration = Duration::from_secs(1);
const SYNC_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(5);
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
const MAX_FEEDBACK_FAILURES: u32 = 3;
const RE_ANCHOR_AFTER: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct Stats {
    pub sent: AtomicU64,
    pub resent: AtomicU64,
    pub expired: AtomicU64,
    pub silence: AtomicU64,
    pub skipped: AtomicU64,
    pub overruns: AtomicU64,
    pub send_errors: AtomicU64,
    pub peak: std::sync::atomic::AtomicU32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// Maps RTP time to the monotonic clock; the anchor moves only after a long scheduler stall.
#[derive(Clone)]
pub struct StreamClock {
    start: Arc<Mutex<Instant>>,
    first_timestamp: u32,
    moved: Arc<Notify>,
}

impl StreamClock {
    pub fn new(start: Instant, first_timestamp: u32) -> Self {
        Self {
            start: Arc::new(Mutex::new(start)),
            first_timestamp,
            moved: Arc::new(Notify::new()),
        }
    }

    pub fn start(&self) -> Instant {
        *lock(&self.start)
    }

    // Also valid before the stream starts, so a sync sent during the pre-roll maps the same timeline.
    pub fn rtp_now(&self, now: Instant) -> u32 {
        let start = self.start();
        if now >= start {
            pacing::rtp_at(self.first_timestamp, now - start)
        } else {
            self.first_timestamp
                .wrapping_sub(pacing::duration_to_frames(start - now) as u32)
        }
    }

    // The receiver keeps the old mapping until it gets a sync, so the control loop is woken to send one.
    fn re_anchor(&self, index: u64, now: Instant) {
        *lock(&self.start) = now.checked_sub(pacing::packet_offset(index)).unwrap_or(now);
        self.moved.notify_one();
    }
}

struct CaptureThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureThread {
    fn spawn(
        frames: mpsc::Sender<Vec<u8>>,
        mut pool: mpsc::Receiver<Vec<u8>>,
        depth: Arc<AtomicUsize>,
        stats: Arc<Stats>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let handle = std::thread::Builder::new()
            .name("rustplay-capture".into())
            .spawn(move || capture_loop(&thread_stop, &frames, &mut pool, &depth, &stats, &ready_tx))
            .map_err(Error::Io)?;
        let mut thread = Self {
            stop,
            handle: Some(handle),
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(thread),
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

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn capture_loop(
    stop: &AtomicBool,
    frames: &mpsc::Sender<Vec<u8>>,
    pool: &mut mpsc::Receiver<Vec<u8>>,
    depth: &AtomicUsize,
    stats: &Stats,
    ready: &std_mpsc::Sender<Result<()>>,
) {
    let mut capture = match LoopbackCapture::start() {
        Ok(capture) => {
            let _ = ready.send(Ok(()));
            Some(capture)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut encoder = AlacEncoder::new(SPF as usize);
    let mut pcm = [0i16; PCM_SAMPLES];
    let mut filled = 0usize;
    let mut empty_reads = 0u32;
    while !stop.load(Ordering::Acquire) {
        let Some(active) = capture.as_mut() else {
            std::thread::sleep(RESTART_DELAY);
            match LoopbackCapture::start() {
                Ok(c) => {
                    tracing::info!("captura de audio restablecida");
                    capture = Some(c);
                    filled = 0;
                }
                Err(e) => tracing::debug!("no se pudo reiniciar la captura de audio: {e}"),
            }
            continue;
        };
        match active.read(&mut pcm[filled * 2..]) {
            Ok(0) => {
                // Pad a partial frame only after ~100 ms without data; a single empty read is just capture jitter.
                empty_reads += 1;
                if filled == 0 || empty_reads < 5 {
                    continue;
                }
                pcm[filled * 2..].fill(0);
                filled = SPF as usize;
            }
            Ok(n) => {
                empty_reads = 0;
                filled += n;
            }
            Err(e) => {
                tracing::warn!("error en la captura de audio ({e}); se reintentará");
                capture = None;
                filled = 0;
                continue;
            }
        }
        if filled < SPF as usize {
            continue;
        }
        filled = 0;
        let peak = pcm.iter().map(|s| u32::from(s.unsigned_abs())).max().unwrap_or(0);
        stats.peak.fetch_max(peak, Ordering::Relaxed);
        let Ok(mut buffer) = pool.try_recv() else {
            stats.overruns.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        buffer.clear();
        if pcm_mode() {
            buffer.extend(pcm.iter().flat_map(|s| s.to_be_bytes()));
        } else if let Err(e) = encoder.encode(&pcm, &mut buffer) {
            tracing::warn!("error del codificador ALAC: {e}");
            continue;
        }
        depth.fetch_add(1, Ordering::AcqRel);
        if frames.try_send(buffer).is_err() {
            depth.fetch_sub(1, Ordering::AcqRel);
            stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn encode_silence() -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(FRAME_CAPACITY);
    if pcm_mode() {
        out.resize(PCM_SAMPLES * 2, 0);
    } else {
        AlacEncoder::new(SPF as usize).encode(&[0i16; PCM_SAMPLES], &mut out)?;
    }
    Ok(out)
}

// Diagnostic: RUSTPLAY_TX_PCM=1 sends big-endian LPCM (ct=1) instead of ALAC.
pub fn pcm_mode() -> bool {
    static PCM: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PCM.get_or_init(|| std::env::var_os("RUSTPLAY_TX_PCM").is_some_and(|v| v == "1"))
}

pub struct PipelineParams {
    pub data_socket: UdpSocket,
    pub control_socket: UdpSocket,
    pub data_peer: SocketAddr,
    pub control_peer: SocketAddr,
    pub latency_samples: u32,
    pub builder: PacketBuilder,
    pub silence: Vec<u8>,
    pub redundancy: bool,
    pub sync_clock: SyncClock,
    pub sync_sequence: u16,
    pub feedback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncClock {
    Wall,
    Boot,
}

impl SyncClock {
    fn now(self) -> u64 {
        match self {
            Self::Wall => NtpTimestamp::now().0,
            Self::Boot => clock::ntp_boot_timestamp(),
        }
    }
}

pub struct Pipeline {
    stop: watch::Sender<bool>,
    capture: Option<CaptureThread>,
    tasks: Vec<TaskHandle<()>>,
    closed: watch::Receiver<bool>,
    pub stats: Arc<Stats>,
}

// The capture thread is started first so that audio-device errors surface before the receiver is told to RECORD.
pub struct PendingCapture {
    capture: CaptureThread,
    frames: mpsc::Receiver<Vec<u8>>,
    pool: mpsc::Sender<Vec<u8>>,
    depth: Arc<AtomicUsize>,
    stats: Arc<Stats>,
}

impl PendingCapture {
    pub fn start() -> Result<Self> {
        let (frames_tx, frames_rx) = mpsc::channel(POOL_FRAMES);
        let (pool_tx, pool_rx) = mpsc::channel(POOL_FRAMES);
        for _ in 0..POOL_FRAMES {
            let _ = pool_tx.try_send(Vec::with_capacity(FRAME_CAPACITY));
        }
        let depth = Arc::new(AtomicUsize::new(0));
        let stats = Arc::new(Stats::default());
        let capture = CaptureThread::spawn(frames_tx, pool_rx, Arc::clone(&depth), Arc::clone(&stats))?;
        Ok(Self {
            capture,
            frames: frames_rx,
            pool: pool_tx,
            depth,
            stats,
        })
    }

    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    pub fn launch(self, params: PipelineParams, conn: Arc<tokio::sync::Mutex<RtspConn>>) -> Pipeline {
        let PipelineParams {
            data_socket,
            control_socket,
            data_peer,
            control_peer,
            latency_samples,
            builder,
            silence,
            redundancy,
            sync_clock,
            sync_sequence,
            feedback,
        } = params;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (closed_tx, closed_rx) = watch::channel(false);
        let ring = Arc::new(Mutex::new(PacketRing::new()));
        let start = Instant::now() + PRE_ROLL;
        let clock = StreamClock::new(start, builder.next_timestamp());
        let stats = self.stats;
        let sender = SendLoop {
            socket: data_socket,
            peer: data_peer,
            ring: Arc::clone(&ring),
            builder,
            frames: self.frames,
            pool: self.pool,
            depth: self.depth,
            silence,
            clock: clock.clone(),
            stats: Arc::clone(&stats),
            pacer: Pacer::default(),
            redundancy,
            built: 0,
        };
        let control = ControlLoop {
            socket: control_socket,
            peer: control_peer,
            ring,
            clock,
            latency_samples,
            stats: Arc::clone(&stats),
            sync_clock,
            sync_sequence,
        };
        let mut tasks = vec![
            tokio::spawn(sender.run(stop_rx.clone())),
            tokio::spawn(control.run(stop_rx.clone())),
        ];
        if feedback {
            tasks.push(tokio::spawn(feedback_loop(conn, stop_rx, closed_tx)));
        }
        Pipeline {
            stop: stop_tx,
            capture: Some(self.capture),
            tasks,
            closed: closed_rx,
            stats,
        }
    }
}

impl Pipeline {
    // Resolves when the receiver stops answering keep-alive requests.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }

    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        if let Some(mut capture) = self.capture.take() {
            capture.shutdown();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct SendLoop {
    socket: UdpSocket,
    peer: SocketAddr,
    ring: Arc<Mutex<PacketRing>>,
    builder: PacketBuilder,
    frames: mpsc::Receiver<Vec<u8>>,
    pool: mpsc::Sender<Vec<u8>>,
    depth: Arc<AtomicUsize>,
    silence: Vec<u8>,
    clock: StreamClock,
    stats: Arc<Stats>,
    pacer: Pacer,
    redundancy: bool,
    built: u64,
}

impl SendLoop {
    async fn run(mut self, mut stop: watch::Receiver<bool>) {
        // Anything captured while SETUP was in progress is stale.
        while let Ok(buffer) = self.frames.try_recv() {
            self.depth.fetch_sub(1, Ordering::AcqRel);
            let _ = self.pool.try_send(buffer);
        }
        let mut index = 0u64;
        loop {
            let deadline = self.clock.start() + pacing::packet_offset(index);
            let now = Instant::now();
            if now >= deadline {
                if now - deadline > RE_ANCHOR_AFTER {
                    self.clock.re_anchor(index, now);
                }
                self.send_next();
                index += 1;
                continue;
            }
            tokio::select! {
                () = sleep_until(deadline) => {}
                _ = stop.changed() => break,
            }
        }
    }

    fn take_frame(&mut self) -> Option<Vec<u8>> {
        let frame = self.frames.try_recv().ok()?;
        self.depth.fetch_sub(1, Ordering::AcqRel);
        Some(frame)
    }

    fn send_next(&mut self) {
        let mut action = self.pacer.decide(self.depth.load(Ordering::Acquire));
        while action == Action::Skip {
            if let Some(dropped) = self.take_frame() {
                let _ = self.pool.try_send(dropped);
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
            action = self.pacer.decide(self.depth.load(Ordering::Acquire));
        }
        let frame = if action == Action::Send { self.take_frame() } else { None };
        if frame.is_none() {
            self.stats.silence.fetch_add(1, Ordering::Relaxed);
        }
        let payload = frame.as_deref().unwrap_or(&self.silence);
        let mut ring = lock(&self.ring);
        let slot = ring.slot_mut(self.builder.next_sequence());
        match self.builder.build(slot, payload) {
            Ok((sequence, len)) => {
                ring.commit(sequence, len);
                if self.redundancy {
                    for back in [2u16, 1] {
                        if self.built >= u64::from(back)
                            && let Some(previous) = ring.get(sequence.wrapping_sub(back))
                        {
                            let _ = self.socket.try_send_to(previous, self.peer);
                        }
                    }
                }
                self.built += 1;
                if let Some(packet) = ring.get(sequence) {
                    match self.socket.try_send_to(packet, self.peer) {
                        Ok(_) => {
                            self.stats.sent.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            self.stats.send_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("no se pudo construir el paquete de audio: {e}"),
        }
        drop(ring);
        if let Some(buffer) = frame {
            let _ = self.pool.try_send(buffer);
        }
    }
}

struct ControlLoop {
    socket: UdpSocket,
    peer: SocketAddr,
    ring: Arc<Mutex<PacketRing>>,
    clock: StreamClock,
    latency_samples: u32,
    sync_clock: SyncClock,
    sync_sequence: u16,
    stats: Arc<Stats>,
}

impl ControlLoop {
    fn send_sync(&self, first: bool) {
        let playhead = self
            .clock
            .rtp_now(Instant::now())
            .wrapping_sub(self.latency_samples);
        let mut packet = [0u8; SYNC_LEN];
        rtp::write_sync(&mut packet, first, playhead, self.sync_clock.now(), playhead);
        packet[2..4].copy_from_slice(&self.sync_sequence.to_be_bytes());
        if let Err(e) = self.socket.try_send_to(&packet, self.peer) {
            tracing::debug!("no se pudo enviar el paquete de sincronización: {e}");
        }
    }

    fn answer(&self, packet: &[u8], from: SocketAddr, scratch: &mut [u8]) {
        let Some(request) = rtp::parse_resend_request(packet) else {
            let head: Vec<u8> = packet.iter().take(8).copied().collect();
            tracing::debug!(%from, len = packet.len(), ?head, "paquete de control del receptor");
            return;
        };
        let ring = lock(&self.ring);
        let outcome = rtp::serve_resend(&ring, &request, scratch, |datagram| {
            let _ = self.socket.try_send_to(datagram, from);
        });
        drop(ring);
        self.stats.resent.fetch_add(u64::from(outcome.resent), Ordering::Relaxed);
        self.stats.expired.fetch_add(u64::from(outcome.expired), Ordering::Relaxed);
    }

    async fn run(self, mut stop: watch::Receiver<bool>) {
        let mut sync_tick = interval(SYNC_INTERVAL);
        sync_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut stats_tick = interval(STATS_INTERVAL);
        stats_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        stats_tick.reset();
        let mut first = true;
        let mut buf = [0u8; 128];
        let mut scratch = vec![0u8; SLOT_SIZE + 8];
        loop {
            tokio::select! {
                _ = sync_tick.tick() => {
                    self.send_sync(first);
                    first = false;
                }
                () = self.clock.moved.notified() => {
                    self.send_sync(true);
                    sync_tick.reset();
                }
                _ = stats_tick.tick() => self.log_stats(),
                received = self.socket.recv_from(&mut buf) => match received {
                    Ok((len, from)) if from.ip() == self.peer.ip() => self.answer(&buf[..len], from, &mut scratch),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                    Err(e) => {
                        tracing::warn!("error en el socket de control: {e}");
                        break;
                    }
                },
                _ = stop.changed() => break,
            }
        }
    }

    fn log_stats(&self) {
        let s = &self.stats;
        tracing::debug!(
            enviados = s.sent.load(Ordering::Relaxed),
            reenviados = s.resent.load(Ordering::Relaxed),
            caducados = s.expired.load(Ordering::Relaxed),
            silencio = s.silence.load(Ordering::Relaxed),
            descartados = s.skipped.load(Ordering::Relaxed),
            desbordes = s.overruns.load(Ordering::Relaxed),
            errores_de_envio = s.send_errors.load(Ordering::Relaxed),
            pico_capturado = s.peak.swap(0, Ordering::Relaxed),
            "estadísticas de audio"
        );
    }
}

pub(crate) async fn feedback_loop(conn: Arc<tokio::sync::Mutex<RtspConn>>, mut stop: watch::Receiver<bool>, closed: watch::Sender<bool>) {
    let mut tick = interval(FEEDBACK_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut failures = 0u32;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => return,
        }
        let result = tokio::select! {
            result = async { conn.lock().await.request("POST", "/feedback", &[], None, &[]).await } => result,
            _ = stop.changed() => return,
        };
        match result {
            Ok(_) => failures = 0,
            Err(e) => {
                failures += 1;
                tracing::debug!("fallo en /feedback ({failures}): {e}");
                if failures >= MAX_FEEDBACK_FAILURES {
                    let _ = closed.send(true);
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::rtp::NoProtection;

    async fn recv(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
            .await
            .expect("tiempo agotado esperando un datagrama")
            .unwrap();
        buf[..len].to_vec()
    }

    fn sync_fields(packet: &[u8]) -> (u8, u32, u64) {
        (
            packet[0],
            u32::from_be_bytes(packet[4..8].try_into().unwrap()),
            u64::from_be_bytes(packet[8..16].try_into().unwrap()),
        )
    }

    // Seconds between the instant a sync packet was stamped and the playout of `timestamp` it announces.
    fn lead_seconds(sync: &[u8], sent: u64, timestamp: u32) -> f64 {
        let (_, playhead, ntp) = sync_fields(sync);
        let ahead = f64::from(timestamp.wrapping_sub(playhead) as i32) / 44_100.0;
        ahead - (sent.wrapping_sub(ntp) as i64) as f64 / 4_294_967_296.0
    }

    #[test]
    fn stream_clock_extends_the_timeline_before_the_start() {
        let start = Instant::now() + Duration::from_secs(10);
        let clock = StreamClock::new(start, 1000);
        assert_eq!(clock.rtp_now(start), 1000);
        assert_eq!(clock.rtp_now(start + Duration::from_secs(1)), 45_100);
        assert_eq!(clock.rtp_now(start - Duration::from_millis(40)), 1000u32.wrapping_sub(1764));
        assert_eq!(clock.rtp_now(start - Duration::from_secs(1)), 1000u32.wrapping_sub(44_100));
    }

    #[tokio::test]
    async fn sync_sent_during_the_pre_roll_announces_the_full_latency() {
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let first_timestamp = 0xFFFF_FF00u32;
        let latency_samples = 7497;
        let start = Instant::now() + Duration::from_millis(200);
        let clock = StreamClock::new(start, first_timestamp);
        let control = ControlLoop {
            socket,
            peer: receiver.local_addr().unwrap(),
            ring: Arc::new(Mutex::new(PacketRing::new())),
            clock: clock.clone(),
            latency_samples,
            sync_clock: SyncClock::Boot,
            sync_sequence: 4,
            stats: Arc::new(Stats::default()),
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(control.run(stop_rx));

        let sync = recv(&receiver).await;
        assert!(Instant::now() < start, "la primera sincronización sale durante el pre-roll");
        assert_eq!((sync[0], sync[1], sync[2], sync[3]), (0x90, 0xD4, 0, 4));
        assert_eq!(sync[4..8], sync[16..20]);
        // The first packet leaves at `start`: its playout must be `latency` after that instant.
        let start_ntp = clock::ntp_boot_timestamp_at(start.into_std());
        let lead = lead_seconds(&sync, start_ntp, first_timestamp);
        let expected = f64::from(latency_samples) / 44_100.0;
        assert!((lead - expected).abs() < 0.002, "adelanto anunciado {lead}, esperado {expected}");

        // A scheduler stall moves the anchor; the receiver is told at once with a forced sync.
        let moved_at = Instant::now() + Duration::from_secs(3);
        clock.re_anchor(0, moved_at);
        let resync = recv(&receiver).await;
        assert_eq!((resync[0], resync[1]), (0x90, 0xD4));
        let lead = lead_seconds(&resync, clock::ntp_boot_timestamp_at(moved_at.into_std()), first_timestamp);
        assert!((lead - expected).abs() < 0.002, "adelanto tras reanclar {lead}, esperado {expected}");

        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn sender_paces_frames_then_silence_and_control_syncs_and_resends() {
        let rx_data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let rx_control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_data = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx_control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control_addr = tx_control.local_addr().unwrap();
        let (frames_tx, frames_rx) = mpsc::channel(POOL_FRAMES);
        let (pool_tx, mut pool_rx) = mpsc::channel(POOL_FRAMES);
        let depth = Arc::new(AtomicUsize::new(0));
        let stats = Arc::new(Stats::default());
        let ring = Arc::new(Mutex::new(PacketRing::new()));
        let first_timestamp = 5000u32;
        let clock = StreamClock::new(Instant::now() + Duration::from_millis(60), first_timestamp);
        let sender = SendLoop {
            socket: tx_data,
            peer: rx_data.local_addr().unwrap(),
            ring: Arc::clone(&ring),
            builder: PacketBuilder::new(65_534, first_timestamp, 9, Box::new(NoProtection)),
            frames: frames_rx,
            pool: pool_tx,
            depth: Arc::clone(&depth),
            silence: vec![0xAA; 8],
            clock: clock.clone(),
            stats: Arc::clone(&stats),
            pacer: Pacer::default(),
            redundancy: false,
            built: 0,
        };
        let control = ControlLoop {
            socket: tx_control,
            peer: rx_control.local_addr().unwrap(),
            ring,
            clock,
            latency_samples: 13_230,
            sync_clock: SyncClock::Wall,
            sync_sequence: 7,
            stats: Arc::clone(&stats),
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let sender = tokio::spawn(sender.run(stop_rx.clone()));
        let control = tokio::spawn(control.run(stop_rx));
        tokio::time::sleep(Duration::from_millis(10)).await;
        for i in 0..4u8 {
            depth.fetch_add(1, Ordering::AcqRel);
            frames_tx.try_send(vec![i; 20]).unwrap();
        }

        let started = tokio::time::Instant::now();
        let mut packets = Vec::new();
        for _ in 0..8 {
            packets.push(recv(&rx_data).await);
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(40), "los paquetes deben ir al ritmo de 8 ms: {elapsed:?}");
        let timestamps: Vec<u32> = packets.iter().map(|p| u32::from_be_bytes(p[4..8].try_into().unwrap())).collect();
        let sequences: Vec<u16> = packets.iter().map(|p| u16::from_be_bytes(p[2..4].try_into().unwrap())).collect();
        assert_eq!(sequences, [65_534, 65_535, 0, 1, 2, 3, 4, 5]);
        for (i, ts) in timestamps.iter().enumerate() {
            assert_eq!(*ts, first_timestamp + 352 * i as u32);
        }
        assert_eq!(packets[0][1], 0x60);
        assert_eq!(packets[1][1], 0x60);
        // Four frames were queued before the first deadline, so they go out first, then silence.
        assert_eq!(&packets[0][12..], &[0u8; 20][..]);
        assert_eq!(&packets[3][12..], &[3u8; 20][..]);
        assert_eq!(&packets[7][12..], &[0xAA; 8][..]);
        assert!(pool_rx.try_recv().is_ok(), "los búferes usados vuelven al fondo común");

        let sync = recv(&rx_control).await;
        assert_eq!(sync.len(), SYNC_LEN);
        assert_eq!((sync[0], sync[1]), (0x90, 0xD4));
        let playhead = u32::from_be_bytes(sync[4..8].try_into().unwrap());
        let head = u32::from_be_bytes(sync[16..20].try_into().unwrap());
        assert_eq!(head, playhead);

        let mut request = [0u8; 8];
        rustplay_core::rtp::encode_resend_request(3, 65_535, 2, &mut request);
        rx_control.send_to(&request, control_addr).await.unwrap();
        for (i, wanted) in [65_535u16, 0].into_iter().enumerate() {
            let response = recv(&rx_control).await;
            assert_eq!(response[..4], [0x80, 0xD6, 0, 3], "respuesta {i}");
            assert_eq!(&response[4..], &packets[1 + i][..]);
            assert_eq!(u16::from_be_bytes(response[6..8].try_into().unwrap()), wanted);
        }
        assert_eq!(stats.resent.load(Ordering::Relaxed), 2);
        assert!(stats.silence.load(Ordering::Relaxed) >= 4);

        stop_tx.send(true).unwrap();
        sender.await.unwrap();
        control.await.unwrap();
    }
}
