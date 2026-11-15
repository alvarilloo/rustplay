use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use aes::Aes128;
use rustplay_core::crypto::stream::AesCtrKey;
use rustplay_core::media::gpu::GpuDevice;
use rustplay_core::media::video::{DecodedFrame, H264Decoder, OverlayTexts, Presenter, PresenterOptions};
use rustplay_core::{Error, Result};
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc as async_mpsc;
use tokio_util::sync::CancellationToken;

use crate::devices::{Control, DeviceKey, Windows};
use crate::capture::Controller;
use crate::{Live, LiveConfig, RxEvent};

pub const HEADER_LEN: usize = 128;
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
const POOL_SIZE: usize = 8;
const INITIAL_UNIT_CAPACITY: usize = 256 * 1024;
const SKIP_CHUNK: usize = 64 * 1024;
const IDR_WAIT_STEPS: u32 = 100;
const IDR_WAIT_STEP: Duration = Duration::from_millis(2);
const IDLE_TICK: Duration = Duration::from_millis(50);
// How long a window waits for a session that was handed over to it.
const HANDOVER_WAIT: Duration = Duration::from_secs(2);
const START_CODE: [u8; 4] = [0, 0, 0, 1];
const START_CODE_SHORT: [u8; 3] = [0, 0, 1];

const KIND_VIDEO: u8 = 0;
const KIND_CODEC: u8 = 1;
const VARIANT_IDR: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub payload_size: usize,
    pub kind: u8,
    pub variant: u8,
    pub option: u16,
    pub timestamp: u64,
}

impl Header {
    pub fn parse(raw: &[u8; HEADER_LEN]) -> Self {
        Self {
            payload_size: u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize,
            kind: raw[4],
            variant: raw[5],
            option: u16::from_le_bytes([raw[6], raw[7]]),
            timestamp: u64::from_le_bytes([
                raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
            ]),
        }
    }

    pub fn is_idr(&self) -> bool {
        self.variant == VARIANT_IDR
    }
}

fn float_at(raw: &[u8; HEADER_LEN], offset: usize) -> f32 {
    f32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

pub fn codec_dimensions(raw: &[u8; HEADER_LEN]) -> Option<(u32, u32)> {
    [(16, 20), (40, 44), (56, 60)]
        .into_iter()
        .map(|(w, h)| (float_at(raw, w), float_at(raw, h)))
        .find(|(w, h)| w.is_finite() && h.is_finite() && *w >= 1.0 && *h >= 1.0)
        .map(|(w, h)| (w as u32, h as u32))
}

pub fn ntp_to_100ns(timestamp: u64) -> i64 {
    let seconds = timestamp >> 32;
    let fraction = timestamp & 0xFFFF_FFFF;
    let ticks = seconds * 10_000_000 + ((fraction * 10_000_000) >> 32);
    ticks as i64
}

pub struct MirrorCipher {
    inner: Ctr128BE<Aes128>,
}

impl MirrorCipher {
    pub fn new(key: &AesCtrKey) -> Self {
        Self {
            inner: Ctr128BE::<Aes128>::new(&key.key.into(), &key.iv.into()),
        }
    }

    pub fn decrypt_in_place(&mut self, data: &mut [u8]) {
        self.inner.apply_keystream(data);
    }
}

pub fn parse_avcc(payload: &[u8], out: &mut Vec<u8>) -> Result<u8> {
    let bad = || Error::protocol("paquete de códec H.264 (avcC) no válido");
    if payload.len() < 7 || payload[0] != 1 {
        return Err(bad());
    }
    let length_size = (payload[4] & 0x03) + 1;
    out.clear();
    let mut pos = 5;
    let mut sets = 0;
    let sps_count = usize::from(payload[pos] & 0x1F);
    pos += 1;
    for _ in 0..sps_count {
        pos = append_parameter_set(payload, pos, out).ok_or_else(bad)?;
        sets += 1;
    }
    let pps_count = usize::from(*payload.get(pos).ok_or_else(bad)?);
    pos += 1;
    for _ in 0..pps_count {
        pos = append_parameter_set(payload, pos, out).ok_or_else(bad)?;
        sets += 1;
    }
    if sps_count == 0 || pps_count == 0 || sets == 0 {
        return Err(bad());
    }
    Ok(length_size)
}

fn append_parameter_set(payload: &[u8], pos: usize, out: &mut Vec<u8>) -> Option<usize> {
    let len_bytes = payload.get(pos..pos + 2)?;
    let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
    let start = pos + 2;
    let set = payload.get(start..start + len)?;
    if set.is_empty() {
        return None;
    }
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(set);
    Some(start + len)
}

pub fn avcc_to_annexb(data: &mut [u8], length_size: u8) -> Result<usize> {
    let width = match length_size {
        3 => 3,
        4 => 4,
        other => {
            return Err(Error::protocol(format!(
                "tamaño de prefijo de NAL no admitido: {other}"
            )));
        }
    };
    let mut pos = 0;
    let mut count = 0;
    while pos < data.len() {
        let Some(field) = data.get(pos..pos + width) else {
            return Err(Error::protocol("NAL truncado en el paquete de vídeo"));
        };
        let len = field
            .iter()
            .fold(0usize, |acc, &byte| (acc << 8) | usize::from(byte));
        let start = pos + width;
        if len == 0 || len > data.len() - start {
            return Err(Error::protocol("longitud de NAL no válida"));
        }
        if data[start] & 0x80 != 0 {
            return Err(Error::protocol(
                "NAL no válido (posible clave de descifrado incorrecta)",
            ));
        }
        if width == 4 {
            data[pos..start].copy_from_slice(&START_CODE);
        } else {
            data[pos..start].copy_from_slice(&START_CODE_SHORT);
        }
        pos = start + len;
        count += 1;
    }
    Ok(count)
}

struct Unit {
    buf: Vec<u8>,
    pts: i64,
}

pub struct MirrorWindow {
    pub live: Arc<Live>,
    pub prewarm: Arc<Prewarm>,
    pub windows: Arc<Windows<Attach>>,
    // Finds the window of a device that comes back.
    pub key: DeviceKey,
    // The sender's name, for the "screen off" message.
    pub device: String,
    // Set while the user muted this device from its window (shared with its audio renderer).
    pub mute: Arc<AtomicBool>,
}

// Header byte 6 of a codec packet: 0x56/0x5e = the sender stopped video (screen locked),
// 0x16/0x1e = video resumed.
pub fn suspend_signal(raw: &[u8; HEADER_LEN]) -> Option<bool> {
    match raw[6] {
        0x56 | 0x5e => Some(true),
        0x16 | 0x1e => Some(false),
        _ => None,
    }
}

struct VideoShared {
    stop: AtomicBool,
    suspended: AtomicBool,
    // Set before `stop` when the session ended without the user asking, so the window may
    // wait for the device to come back.
    park: AtomicBool,
    disconnect: CancellationToken,
}

// A mirroring session handed to a window thread.
pub struct Attach {
    units: Receiver<Unit>,
    free: SyncSender<Vec<u8>>,
    shared: Arc<VideoShared>,
    events: async_mpsc::Sender<RxEvent>,
    peer: SocketAddr,
    title: String,
    device: String,
    mute: Arc<AtomicBool>,
}

// What a window thread needs besides its sessions.
struct WindowContext {
    id: u64,
    live: Arc<Live>,
    prewarm: Arc<Prewarm>,
    windows: Arc<Windows<Attach>>,
}

pub struct MirrorStream {
    pub data_port: u16,
    cancel: CancellationToken,
    shared: Arc<VideoShared>,
}

impl MirrorStream {
    pub async fn start(
        key: &AesCtrKey,
        peer: SocketAddr,
        title: String,
        window: MirrorWindow,
        events: async_mpsc::Sender<RxEvent>,
        parent: &CancellationToken,
        disconnect: CancellationToken,
    ) -> Result<Self> {
        let listener = TcpListener::bind((rustplay_core::net::listen_addr(peer.ip()), 0)).await?;
        let data_port = listener.local_addr()?.port();
        let cancel = parent.child_token();
        let shared = Arc::new(VideoShared {
            stop: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            park: AtomicBool::new(false),
            disconnect,
        });

        let (unit_tx, unit_rx) = mpsc::sync_channel::<Unit>(POOL_SIZE);
        let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            let _ = free_tx.try_send(Vec::with_capacity(INITIAL_UNIT_CAPACITY));
        }
        let attach = Attach {
            units: unit_rx,
            free: free_tx.clone(),
            shared: Arc::clone(&shared),
            events: events.clone(),
            peer,
            title,
            device: window.device.clone(),
            mute: Arc::clone(&window.mute),
        };
        match window.windows.claim(&window.key, attach) {
            Ok(_) => tracing::info!(%peer, "«{}» vuelve a duplicar: se reutiliza su ventana", window.device),
            Err(attach) => open_window(window, attach)?,
        }

        let state = ReceiveState {
            cipher: MirrorCipher::new(key),
            params: Vec::with_capacity(1024),
            pending_params: false,
            length_size: 4,
            dimensions: None,
            suspended: false,
            video_shared: Arc::clone(&shared),
            unit_tx,
            free_tx,
            free_rx,
            scratch: Vec::with_capacity(SKIP_CHUNK),
            events,
            peer,
            received: 0,
            dropped: 0,
            invalid: 0,
        };
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let mut state = state;
            tokio::select! {
                () = task_cancel.cancelled() => {}
                () = serve(listener, peer.ip(), &mut state) => {}
            }
            tracing::debug!(
                %peer,
                received = state.received,
                dropped = state.dropped,
                invalid = state.invalid,
                "recepción de vídeo detenida"
            );
            if state.dimensions.is_some() {
                let _ = state.events.try_send(RxEvent::MirrorStopped { peer });
            }
        });

        Ok(Self {
            data_port,
            cancel,
            shared,
        })
    }

    // The sender had its screen off when the stream ended.
    pub fn suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::Acquire)
    }

    // Ends the stream. With `lost` (the sender went away without asking), the window stays
    // open for a while in case the device comes back.
    pub fn end(self, lost: bool) {
        if lost {
            self.shared.park.store(true, Ordering::Release);
        }
    }
}

impl Drop for MirrorStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.shared.stop.store(true, Ordering::Release);
    }
}

fn open_window(window: MirrorWindow, attach: Attach) -> Result<()> {
    let (control_tx, control_rx) = mpsc::channel();
    let id = window.windows.insert(window.key.clone(), control_tx);
    let ctx = WindowContext {
        id,
        live: window.live,
        prewarm: window.prewarm,
        windows: Arc::clone(&window.windows),
    };
    let spawned = std::thread::Builder::new()
        .name("rustplay-video".into())
        .spawn(move || window_thread(&ctx, attach, &control_rx));
    match spawned {
        Ok(thread) => {
            window.windows.set_thread(id, thread);
            Ok(())
        }
        Err(e) => {
            window.windows.remove(id);
            Err(e.into())
        }
    }
}

struct ReceiveState {
    cipher: MirrorCipher,
    params: Vec<u8>,
    pending_params: bool,
    length_size: u8,
    dimensions: Option<(u32, u32)>,
    suspended: bool,
    video_shared: Arc<VideoShared>,
    unit_tx: SyncSender<Unit>,
    free_tx: SyncSender<Vec<u8>>,
    free_rx: Receiver<Vec<u8>>,
    scratch: Vec<u8>,
    events: async_mpsc::Sender<RxEvent>,
    peer: SocketAddr,
    received: u64,
    dropped: u64,
    invalid: u64,
}

async fn serve(listener: TcpListener, peer_ip: IpAddr, state: &mut ReceiveState) {
    let stream = loop {
        match listener.accept().await {
            Ok((stream, from)) if from.ip() == peer_ip => break stream,
            Ok((_, from)) => tracing::debug!(%from, "conexión de vídeo ignorada: origen distinto"),
            Err(e) => {
                tracing::warn!("no se pudo aceptar la conexión de vídeo: {e}");
                return;
            }
        }
    };
    drop(listener);
    let _ = stream.set_nodelay(true);
    tracing::debug!("conexión de duplicación de pantalla aceptada");
    match state.run(stream).await {
        Ok(()) => {}
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            tracing::debug!("el cliente cerró la conexión de vídeo");
        }
        Err(e) => tracing::warn!("conexión de vídeo cerrada con error: {e}"),
    }
}

impl ReceiveState {
    async fn run(&mut self, mut stream: TcpStream) -> Result<()> {
        let mut raw = [0u8; HEADER_LEN];
        loop {
            stream.read_exact(&mut raw).await?;
            let header = Header::parse(&raw);
            if header.payload_size > MAX_PAYLOAD {
                return Err(Error::protocol(format!(
                    "paquete de vídeo demasiado grande ({} bytes)",
                    header.payload_size
                )));
            }
            match header.kind {
                KIND_VIDEO => self.video(&mut stream, &header).await?,
                KIND_CODEC => self.codec(&mut stream, &header, &raw).await?,
                _ => self.skip(&mut stream, header.payload_size).await?,
            }
        }
    }

    async fn skip(&mut self, stream: &mut TcpStream, mut remaining: usize) -> Result<()> {
        while remaining > 0 {
            let chunk = remaining.min(SKIP_CHUNK);
            self.scratch.resize(chunk, 0);
            stream.read_exact(&mut self.scratch[..chunk]).await?;
            remaining -= chunk;
        }
        Ok(())
    }

    async fn codec(
        &mut self,
        stream: &mut TcpStream,
        header: &Header,
        raw: &[u8; HEADER_LEN],
    ) -> Result<()> {
        self.scratch.clear();
        self.scratch.resize(header.payload_size, 0);
        stream.read_exact(&mut self.scratch).await?;
        if self.scratch.len() >= 8 && &self.scratch[4..8] == b"hvc1" {
            tracing::warn!("el cliente envía vídeo HEVC, que no está admitido");
            return Ok(());
        }
        if let Some(suspend) = suspend_signal(raw)
            && suspend != self.suspended
        {
            self.suspended = suspend;
            self.video_shared.suspended.store(suspend, Ordering::Release);
            let peer = self.peer;
            tracing::debug!(%peer, suspend, "el cliente cambia el estado del vídeo");
            let _ = self.events.try_send(if suspend {
                RxEvent::MirrorSuspended { peer }
            } else {
                RxEvent::MirrorResumed { peer }
            });
        }
        match parse_avcc(&self.scratch, &mut self.params) {
            Ok(length_size) => {
                self.length_size = length_size;
                self.pending_params = true;
            }
            Err(e) => {
                tracing::warn!("{e}");
                return Ok(());
            }
        }
        if let Some(size) = codec_dimensions(raw)
            && self.dimensions != Some(size)
        {
            self.dimensions = Some(size);
            let _ = self.events.try_send(RxEvent::MirrorStarted {
                peer: self.peer,
                width: size.0,
                height: size.1,
            });
        }
        Ok(())
    }

    async fn acquire(&mut self, wait: bool) -> Option<Vec<u8>> {
        for _ in 0..IDR_WAIT_STEPS {
            match self.free_rx.try_recv() {
                Ok(buf) => return Some(buf),
                Err(TryRecvError::Empty) if wait => tokio::time::sleep(IDR_WAIT_STEP).await,
                Err(_) => return None,
            }
        }
        None
    }

    async fn video(&mut self, stream: &mut TcpStream, header: &Header) -> Result<()> {
        self.received += 1;
        let offset = if self.pending_params {
            self.params.len()
        } else {
            0
        };
        let acquired = self.acquire(header.is_idr() || offset > 0).await;
        let pooled = acquired.is_some();
        let mut buf = acquired.unwrap_or_else(|| std::mem::take(&mut self.scratch));
        buf.clear();
        buf.resize(offset + header.payload_size, 0);
        let read = stream.read_exact(&mut buf[offset..]).await;
        if let Err(e) = read {
            self.recycle(buf, pooled);
            return Err(e.into());
        }
        self.cipher.decrypt_in_place(&mut buf[offset..]);
        if !pooled {
            self.dropped += 1;
            tracing::debug!("sin búfer de vídeo libre: fotograma descartado");
            self.scratch = buf;
            return Ok(());
        }
        if let Err(e) = avcc_to_annexb(&mut buf[offset..], self.length_size) {
            self.invalid += 1;
            if self.invalid == 1 || self.invalid % 100 == 0 {
                tracing::warn!("fotograma de vídeo descartado ({}): {e}", self.invalid);
            }
            self.recycle(buf, true);
            return Ok(());
        }
        if offset > 0 {
            buf[..offset].copy_from_slice(&self.params);
            self.pending_params = false;
        }
        let unit = Unit {
            buf,
            pts: ntp_to_100ns(header.timestamp),
        };
        if let Err(e) = self.unit_tx.try_send(unit) {
            self.dropped += 1;
            let unit = match e {
                mpsc::TrySendError::Full(unit) | mpsc::TrySendError::Disconnected(unit) => unit,
            };
            self.recycle(unit.buf, true);
        }
        Ok(())
    }

    fn recycle(&mut self, buf: Vec<u8>, pooled: bool) {
        if pooled {
            let _ = self.free_tx.try_send(buf);
        } else {
            self.scratch = buf;
        }
    }
}

type Warm = (Arc<GpuDevice>, H264Decoder);

// A GPU device and an H.264 decoder created ahead of time, so the first image of a mirroring
// session does not wait for them.
pub struct Prewarm {
    slot: Mutex<Option<Warm>>,
    filling: AtomicBool,
}

impl Prewarm {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { slot: Mutex::new(None), filling: AtomicBool::new(false) })
    }

    // Prepares a device and a decoder in the background unless one is ready or on its way.
    pub fn fill(self: &Arc<Self>) {
        if self.slot.lock().is_ok_and(|slot| slot.is_some()) || self.filling.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("rustplay-video-prewarm".into()).spawn(move || {
            let started = Instant::now();
            let created = GpuDevice::create().and_then(|gpu| Ok((Arc::clone(&gpu), H264Decoder::new(gpu)?)));
            match created {
                Ok(warm) => {
                    if let Ok(mut slot) = this.slot.lock() {
                        *slot = Some(warm);
                    }
                    tracing::debug!("vídeo preparado por adelantado en {} ms", started.elapsed().as_millis());
                }
                Err(e) => tracing::debug!("no se pudo preparar el vídeo por adelantado: {e}"),
            }
            this.filling.store(false, Ordering::Release);
        });
        if spawned.is_err() {
            self.filling.store(false, Ordering::Release);
        }
    }

    fn take(&self) -> Option<Warm> {
        self.slot.lock().ok().and_then(|mut slot| slot.take())
    }
}

fn presenter_options(config: &LiveConfig, device: &str, hardware: bool) -> PresenterOptions {
    let (window, texts) = (&config.window, &config.texts);
    let decoder = if hardware { &texts.decoder_hardware } else { &texts.decoder_software };
    PresenterOptions {
        fullscreen: window.fullscreen,
        always_on_top: window.always_on_top,
        scaling: window.scaling,
        vsync: window.vsync,
        high_quality_scaling: window.high_quality_scaling,
        controls: window.controls,
        open_app_button: window.open_app_button,
        capture_buttons: true,
        stats: window.stats,
        stats_text: texts.stats.replace("{decoder}", decoder),
        borderless: window.borderless,
        blur_background: window.blur_background,
        lock_position: window.lock_position,
        lock_title: texts.lock_title.clone(),
        lock_message: texts.lock_message.replace("{device}", device),
        overlay: OverlayTexts {
            fullscreen: texts.fullscreen.clone(),
            exit_fullscreen: texts.exit_fullscreen.clone(),
            open_app: texts.open_app.clone(),
            stop: texts.stop.clone(),
            record: texts.record.clone(),
            stop_recording: texts.stop_recording.clone(),
            screenshot: texts.screenshot.clone(),
            toast_fullscreen: texts.toast_fullscreen.clone(),
            toast_recording: texts.toast_recording.clone(),
            toast_screenshot: texts.toast_screenshot.clone(),
            toast_mute: texts.toast_mute.clone(),
            toast_blur: texts.toast_blur.clone(),
            toast_cinema: texts.toast_cinema.clone(),
            toast_lock: texts.toast_lock.clone(),
            toast_stats: texts.toast_stats.clone(),
            toast_topmost: texts.toast_topmost.clone(),
            toast_on: texts.toast_on.clone(),
            toast_off: texts.toast_off.clone(),
        },
    }
}

// Options of a window waiting for its device: the backdrop says the connection is paused.
fn parked_options(config: &LiveConfig, device: &str, hardware: bool) -> PresenterOptions {
    let texts = &config.texts;
    PresenterOptions {
        lock_title: texts.paused_title.clone(),
        lock_message: texts.paused_message.replace("{device}", device),
        ..presenter_options(config, device, hardware)
    }
}

struct VideoSink {
    decoder: H264Decoder,
    presenter: Presenter,
    title: String,
}

impl VideoSink {
    fn open(title: &str, window: &WindowContext, device: &str, config: &LiveConfig) -> Result<Self> {
        if let Some((gpu, decoder)) = window.prewarm.take() {
            tracing::debug!("se usa el vídeo preparado por adelantado");
            match Self::build(title, device, config, gpu, decoder) {
                Ok(sink) => return Ok(sink),
                Err(e) => tracing::debug!("el vídeo preparado no sirve ({e}); se crea de nuevo"),
            }
        }
        let gpu = GpuDevice::create()?;
        let decoder = H264Decoder::new(Arc::clone(&gpu))?;
        Self::build(title, device, config, gpu, decoder)
    }

    fn build(title: &str, device: &str, config: &LiveConfig, gpu: Arc<GpuDevice>, decoder: H264Decoder) -> Result<Self> {
        let options = presenter_options(config, device, decoder.is_hardware());
        let presenter = Presenter::new(gpu, title, options)?;
        Ok(Self { decoder, presenter, title: title.to_owned() })
    }

    // A new session starts from a clean decoder; the window and its GPU device stay.
    fn renew_decoder(&mut self) {
        match H264Decoder::new(Arc::clone(&self.presenter.gpu)) {
            Ok(decoder) => self.decoder = decoder,
            Err(e) => tracing::warn!("no se pudo crear un decodificador nuevo; se reutiliza el anterior: {e}"),
        }
    }

    // Decodes every waiting unit (each frame depends on the previous ones) but shows only the
    // newest picture, so a burst of late frames does not add delay. Returns the frames skipped.
    fn show_newest(
        &mut self,
        first: Unit,
        units: &Receiver<Unit>,
        free: &SyncSender<Vec<u8>>,
        errors: &mut u64,
        capture: &mut Controller,
    ) -> Result<u64> {
        let mut latest = None;
        let mut skipped = 0;
        let gpu = Arc::clone(&self.decoder.gpu);
        let mut decode = |unit: Unit, latest: &mut Option<DecodedFrame>| {
            capture.observe(&unit.buf, unit.pts);
            match self.decoder.decode(&unit.buf, unit.pts) {
                Ok(Some(frame)) => {
                    capture.observe_frame(&gpu, &frame);
                    if latest.replace(frame).is_some() {
                        skipped += 1;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    *errors += 1;
                    if *errors == 1 || *errors % 200 == 0 {
                        tracing::warn!("error al decodificar vídeo ({errors}): {e}");
                    }
                }
            }
            let _ = free.try_send(unit.buf);
        };
        decode(first, &mut latest);
        while let Ok(unit) = units.try_recv() {
            decode(unit, &mut latest);
        }
        // Frames that arrive while waiting for the display are folded in too.
        self.presenter.wait_ready();
        while let Ok(unit) = units.try_recv() {
            decode(unit, &mut latest);
        }
        if let Some(frame) = latest {
            self.presenter.present(&frame)?;
        }
        Ok(skipped)
    }
}

// Lets go of a session whose device started a new one (its old connection is stale).
fn detacher(shared: &Arc<VideoShared>) -> Box<dyn Fn() + Send> {
    let shared = Arc::clone(shared);
    Box::new(move || {
        shared.park.store(true, Ordering::Release);
        if !shared.stop.swap(true, Ordering::AcqRel) {
            shared.disconnect.cancel();
        }
    })
}

// Drives one mirror window for as long as it is open: its first session, then every later
// session of the same device that comes back while the window waits.
fn window_thread(ctx: &WindowContext, first: Attach, control: &Receiver<Control<Attach>>) {
    let opened = Instant::now();
    let config = ctx.live.get();
    let mut sink = match rustplay_core::media::init_thread().and_then(|()| VideoSink::open(&first.title, ctx, &first.device, &config)) {
        Ok(sink) => {
            tracing::debug!(peer = %first.peer, "ventana de vídeo lista en {} ms", opened.elapsed().as_millis());
            Some(sink)
        }
        Err(e) => {
            tracing::warn!(peer = %first.peer, "no se pudo iniciar la salida de vídeo: {e}");
            None
        }
    };
    let mut next = Some((first, false));
    while let Some((attach, claimed)) = next.take() {
        ctx.windows.activate(ctx.id, detacher(&attach.shared), claimed);
        if claimed && let Some(sink) = sink.as_mut() {
            sink.renew_decoder();
        }
        let closed = run_session(ctx, &mut sink, &attach);
        let lost = !closed && attach.shared.park.load(Ordering::Acquire);
        let (peer, device) = (attach.peer, attach.device.clone());
        drop(attach);
        if lost && let Some(active) = sink.as_mut() {
            next = wait_for_device(ctx, active, control, peer, &device);
        }
        if next.is_none() && !ctx.windows.retire(ctx.id) {
            // A session of the same device was handed over meanwhile.
            next = match control.recv_timeout(HANDOVER_WAIT) {
                Ok(Control::Attach(attach)) => Some((attach, true)),
                _ => {
                    ctx.windows.remove(ctx.id);
                    None
                }
            };
        }
    }
    drop(sink);
    if ctx.live.get().video {
        ctx.prewarm.fill();
    }
}

// Shows one session until it ends. True when the user closed the window.
fn run_session(ctx: &WindowContext, sink: &mut Option<VideoSink>, attach: &Attach) -> bool {
    let (units, free, shared, events, peer) = (&attach.units, &attach.free, &*attach.shared, &attach.events, attach.peer);
    let started = Instant::now();
    let mut first_frame = true;
    let mut generation = ctx.live.generation();
    let mut config = ctx.live.get();
    if let Some(active) = sink.as_mut() {
        // Back from waiting: the normal texts and the sender's current name.
        active.presenter.apply(presenter_options(&config, &attach.device, active.decoder.is_hardware()));
        if active.title != attach.title {
            active.presenter.set_title(&attach.title);
            active.title.clone_from(&attach.title);
        }
    }
    let mut errors = 0u64;
    let mut skipped = 0u64;
    let mut paused = false;
    let mut closed = false;
    let mut capture = Controller::new(events.clone());
    if sink.is_some() {
        ctx.live.capture.set_mirroring(true);
    }
    while !shared.stop.load(Ordering::Acquire) {
        let Some(active) = sink.as_mut() else {
            match units.recv_timeout(IDLE_TICK) {
                Ok(unit) => {
                    let _ = free.try_send(unit.buf);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => std::thread::sleep(IDLE_TICK),
            }
            continue;
        };
        if active.presenter.is_closed() {
            tracing::info!(%peer, "ventana de duplicación cerrada: se corta la conexión con el emisor");
            shared.disconnect.cancel();
            closed = true;
            break;
        }
        if active.presenter.take_open_request() {
            let _ = events.try_send(RxEvent::OpenApp);
        }
        capture.tick(&ctx.live, &mut active.presenter);
        if active.presenter.take_mute_request() {
            let muted = !attach.mute.fetch_xor(true, Ordering::AcqRel);
            tracing::info!(%peer, muted, "audio del dispositivo silenciado desde su ventana");
        }
        active.presenter.set_muted(attach.mute.load(Ordering::Acquire));
        let now = ctx.live.generation();
        if now != generation {
            generation = now;
            config = ctx.live.get();
            let options = presenter_options(&config, &attach.device, active.decoder.is_hardware());
            active.presenter.apply(options);
            tracing::debug!("opciones de la ventana de duplicación aplicadas");
        }
        let now_paused = shared.suspended.load(Ordering::Acquire);
        if now_paused != paused {
            paused = now_paused;
            if paused
                && config.window.lock_screen_backdrop
                && let Err(e) = active.presenter.show_paused()
            {
                tracing::warn!("no se pudo mostrar la pantalla de bloqueo: {e}");
            }
        }
        let interval = active.presenter.idle_interval();
        let unit = match units.recv_timeout(interval) {
            Ok(unit) => unit,
            Err(e) => {
                if let Err(e) = active.presenter.tick() {
                    tracing::warn!("no se pudo redibujar la ventana de duplicación: {e}");
                }
                // The video connection closed; the session decides what happens to the window.
                if e == RecvTimeoutError::Disconnected {
                    std::thread::sleep(interval);
                }
                continue;
            }
        };
        match active.show_newest(unit, units, free, &mut errors, &mut capture) {
            Ok(n) => {
                skipped += n;
                if first_frame {
                    first_frame = false;
                    tracing::info!(%peer, "primera imagen en pantalla {} ms después de empezar", started.elapsed().as_millis());
                }
            }
            Err(e) => {
                errors += 1;
                if errors == 1 || errors % 200 == 0 {
                    tracing::warn!("error al mostrar vídeo ({errors}): {e}");
                }
            }
        }
    }
    if skipped > 0 {
        tracing::debug!(skipped, "fotogramas decodificados sin mostrar para ir al más reciente");
    }
    ctx.live.capture.set_mirroring(false);
    capture.finish(&ctx.live);
    closed
}

// Keeps the window of a sender that went away, showing "connection paused", until the same
// device mirrors again (returns its session), the user closes it or the time runs out.
fn wait_for_device(
    ctx: &WindowContext,
    sink: &mut VideoSink,
    control: &Receiver<Control<Attach>>,
    peer: SocketAddr,
    device: &str,
) -> Option<(Attach, bool)> {
    let mut config = ctx.live.get();
    if !ctx.windows.park(ctx.id) {
        // The device already started a new session.
        return match control.recv_timeout(HANDOVER_WAIT) {
            Ok(Control::Attach(attach)) => Some((attach, true)),
            _ => None,
        };
    }
    if !config.keep_window.enabled() || !config.video || sink.presenter.is_closed() {
        return None;
    }
    tracing::info!(%peer, "«{device}» se ha desconectado sin dejar de duplicar: la ventana espera a que vuelva");
    let hardware = sink.decoder.is_hardware();
    sink.presenter.apply(parked_options(&config, device, hardware));
    if let Err(e) = sink.presenter.show_paused() {
        tracing::warn!("no se pudo mostrar el aviso de conexión en pausa: {e}");
    }
    let parked = Instant::now();
    let mut generation = ctx.live.generation();
    loop {
        if sink.presenter.is_closed() {
            tracing::info!(%peer, "ventana en espera de «{device}» cerrada por el usuario");
            return None;
        }
        // Nobody to tell about the "open the app" button while no session is attached.
        let _ = sink.presenter.take_open_request();
        let now = ctx.live.generation();
        if now != generation {
            generation = now;
            config = ctx.live.get();
            if !config.video {
                return None;
            }
            sink.presenter.apply(parked_options(&config, device, hardware));
        }
        if config.keep_window.expired(parked.elapsed()) {
            tracing::info!(%peer, "«{device}» no ha vuelto a tiempo: se cierra su ventana");
            return None;
        }
        let wait = config.keep_window.wait(parked.elapsed(), sink.presenter.idle_interval());
        match control.recv_timeout(wait) {
            Ok(Control::Attach(attach)) => return Some((attach, true)),
            Ok(Control::Close) => {
                tracing::info!(%peer, "se cierra la ventana en espera de «{device}»");
                return None;
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = sink.presenter.tick() {
                    tracing::warn!("no se pudo redibujar la ventana en espera: {e}");
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cinema_options_reach_the_presenter() {
        let mut config = LiveConfig::from(&crate::ReceiverConfig::default());
        let defaults = presenter_options(&config, "iPhone", true);
        assert!(defaults.blur_background, "blurred background is on by default");
        assert!(!defaults.borderless && !defaults.lock_position);
        config.window.borderless = true;
        config.window.blur_background = false;
        config.window.lock_position = true;
        config.texts.toast_blur = "Fondo".into();
        config.texts.toast_off = "no".into();
        let options = presenter_options(&config, "iPhone", true);
        assert!(options.borderless && options.lock_position && !options.blur_background);
        assert_eq!(options.overlay.toast_blur, "Fondo");
        assert_eq!(options.overlay.toast_off, "no");
        let parked = parked_options(&config, "iPhone", true);
        assert!(parked.borderless && parked.lock_position, "a parked window keeps its look");
    }

    fn raw_header(size: u32, kind: u8, variant: u8, option: u16, ts: u64) -> [u8; HEADER_LEN] {
        let mut raw = [0u8; HEADER_LEN];
        raw[..4].copy_from_slice(&size.to_le_bytes());
        raw[4] = kind;
        raw[5] = variant;
        raw[6..8].copy_from_slice(&option.to_le_bytes());
        raw[8..16].copy_from_slice(&ts.to_le_bytes());
        raw
    }

    fn avcc(sps: &[&[u8]], pps: &[&[u8]], length_minus_one: u8) -> Vec<u8> {
        let mut out = vec![1, 0x64, 0, 0x1f, 0xFC | length_minus_one, 0xE0 | sps.len() as u8];
        for set in sps {
            out.extend_from_slice(&(set.len() as u16).to_be_bytes());
            out.extend_from_slice(set);
        }
        out.push(pps.len() as u8);
        for set in pps {
            out.extend_from_slice(&(set.len() as u16).to_be_bytes());
            out.extend_from_slice(set);
        }
        out
    }

    #[test]
    fn header_fields_are_parsed() {
        let header = Header::parse(&raw_header(0x01020304, 1, 0x10, 0x0116, 0x1122334455667788));
        assert_eq!(header.payload_size, 0x01020304);
        assert_eq!(header.kind, 1);
        assert!(header.is_idr());
        assert_eq!(header.option, 0x0116);
        assert_eq!(header.timestamp, 0x1122334455667788);
        assert!(!Header::parse(&raw_header(0, 0, 0, 0, 0)).is_idr());
    }

    #[test]
    fn suspend_signal_reads_header_byte_six() {
        let mut raw = raw_header(10, 1, 0, 0, 0);
        assert_eq!(suspend_signal(&raw), None);
        for (b, expected) in [(0x56, Some(true)), (0x5e, Some(true)), (0x16, Some(false)), (0x1e, Some(false)), (0x00, None)] {
            raw[6] = b;
            assert_eq!(suspend_signal(&raw), expected);
        }
    }

    #[test]
    fn codec_dimensions_come_from_float_fields() {
        let mut raw = raw_header(10, 1, 0, 0x16, 0);
        assert_eq!(codec_dimensions(&raw), None);
        raw[16..20].copy_from_slice(&1170f32.to_le_bytes());
        raw[20..24].copy_from_slice(&2532f32.to_le_bytes());
        assert_eq!(codec_dimensions(&raw), Some((1170, 2532)));
        let mut other = raw_header(10, 1, 0, 0x16, 0);
        other[56..60].copy_from_slice(&640f32.to_le_bytes());
        other[60..64].copy_from_slice(&480f32.to_le_bytes());
        assert_eq!(codec_dimensions(&other), Some((640, 480)));
    }

    #[test]
    fn ntp_timestamps_convert_to_100ns() {
        assert_eq!(ntp_to_100ns(0), 0);
        assert_eq!(ntp_to_100ns(2 << 32), 20_000_000);
        assert_eq!(ntp_to_100ns((1 << 32) | (1 << 31)), 15_000_000);
    }

    #[test]
    fn avcc_extracts_sps_and_pps_as_annex_b() {
        let payload = avcc(&[&[0x67, 1, 2], &[0x67, 3]], &[&[0x68, 9]], 3);
        let mut out = vec![0xAA; 4];
        let length_size = parse_avcc(&payload, &mut out).unwrap();
        assert_eq!(length_size, 4);
        assert_eq!(
            out,
            [
                0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x68, 9
            ]
        );
        let short = avcc(&[&[0x67]], &[&[0x68]], 1);
        assert_eq!(parse_avcc(&short, &mut out).unwrap(), 2);
    }

    #[test]
    fn avcc_rejects_malformed_payloads() {
        let mut out = Vec::new();
        assert!(parse_avcc(&[], &mut out).is_err());
        assert!(parse_avcc(&[2, 0, 0, 0, 0xFF, 0xE1, 0, 0], &mut out).is_err());
        let mut truncated = avcc(&[&[0x67, 1, 2, 3]], &[&[0x68]], 3);
        truncated.truncate(9);
        assert!(parse_avcc(&truncated, &mut out).is_err());
        let no_pps = avcc(&[&[0x67]], &[], 3);
        assert!(parse_avcc(&no_pps, &mut out).is_err());
    }

    #[test]
    fn avcc_to_annex_b_handles_multiple_nals_in_place() {
        let mut data = vec![0, 0, 0, 2, 0x06, 1, 0, 0, 0, 3, 0x65, 7, 8];
        assert_eq!(avcc_to_annexb(&mut data, 4).unwrap(), 2);
        assert_eq!(data, [0, 0, 0, 1, 0x06, 1, 0, 0, 0, 1, 0x65, 7, 8]);
        let mut short = vec![0, 0, 2, 0x41, 5];
        assert_eq!(avcc_to_annexb(&mut short, 3).unwrap(), 1);
        assert_eq!(short, [0, 0, 1, 0x41, 5]);
        assert_eq!(avcc_to_annexb(&mut [], 4).unwrap(), 0);
    }

    #[test]
    fn avcc_to_annex_b_rejects_bad_lengths_without_panicking() {
        for bad in [
            vec![0, 0, 0, 9, 0x41, 1],
            vec![0, 0, 0],
            vec![0, 0, 0, 0, 0x41],
            vec![0xFF, 0xFF, 0xFF, 0xFF, 1],
            vec![0, 0, 0, 1, 0xC1],
            vec![0, 0, 0, 1, 0x41, 0, 0],
        ] {
            let mut data = bad.clone();
            assert!(avcc_to_annexb(&mut data, 4).is_err(), "{bad:?}");
        }
        assert!(avcc_to_annexb(&mut [0, 1, 0x41], 2).is_err());
    }

    #[test]
    fn ctr_is_continuous_across_packet_boundaries() {
        let key = AesCtrKey {
            key: [7; 16],
            iv: [9; 16],
        };
        let plain: Vec<u8> = (0..1000u32).map(|i| (i * 31 + 5) as u8).collect();
        let mut encrypted = plain.clone();
        MirrorCipher::new(&key).decrypt_in_place(&mut encrypted);
        assert_ne!(encrypted, plain);

        let mut whole = encrypted.clone();
        MirrorCipher::new(&key).decrypt_in_place(&mut whole);
        assert_eq!(whole, plain);

        let mut split = encrypted.clone();
        let mut cipher = MirrorCipher::new(&key);
        let mut start = 0;
        for len in [1, 15, 16, 17, 3, 100, 0, 31, 200, 5] {
            cipher.decrypt_in_place(&mut split[start..start + len]);
            start += len;
        }
        cipher.decrypt_in_place(&mut split[start..]);
        assert_eq!(split, plain);
    }
}
