use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustplay_core::crypto::stream::{control_keys_from, data_stream_key};
use rustplay_core::rtsp::Response;
use rustplay_core::{Error, Result};
use plist::{Dictionary, Value};
use tokio::net::{TcpSocket, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{BINARY_PLIST, PARAMETERS, Session, StreamHandle};
use crate::MirrorOptions;
use crate::audio::{self, PendingCapture, PipelineParams, SyncClock};
use crate::clock::{self, TimingService};
use crate::conn::{RtspConn, ensure_ok};
use crate::event::EventChannel;
use crate::info::{self, ReceiverInfo};
use crate::mirror::{AudioLaunch, MirrorParams, MirrorPipeline};
use crate::mirror_setup::{self, MirrorReply, MirrorSetup, StreamPorts};
use crate::rtp::PacketBuilder;
use crate::seal::ChaChaProtection;
use crate::setup::{self, Layout};
use crate::video::{DataSender, FrameCipher};
use crate::video_thread::{Shared, VideoParams, VideoThread};

pub const VIDEO_LATENCY: Duration = Duration::from_millis(75);
// 170 ms, the screen-audio lead iOS announces (sync RTP = next RTP - 7497). The Q60DD stops asking
// for packets about 85 ms before their playout time, so doubletake's 85 ms wired lead arrives late.
pub const AUDIO_LATENCY_SAMPLES: u32 = 7497;
const AUDIO_LATENCY_RANGE_MS: (u32, u32) = (50, 2000);
const INFO_TIMEOUT: Duration = Duration::from_secs(3);
const DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const POOL_BUFFERS: usize = 8;
const QUEUE_DEPTH: usize = 4;

struct Built {
    pipeline: MirrorPipeline,
    timing: TimingService,
    events: Option<EventChannel>,
}

struct Plan<'a> {
    options: MirrorOptions,
    peer: SocketAddr,
    setup: MirrorSetup<'a>,
    audio_uri: String,
    session_uuid: String,
    secret: Vec<u8>,
    conn_arc: Arc<tokio::sync::Mutex<RtspConn>>,
    spawn: VideoSpawn,
    info: ReceiverInfo,
}

type VideoSpawn = fn(VideoParams, Arc<Shared>, mpsc::Receiver<Vec<u8>>, mpsc::Sender<crate::video::Unit>) -> Result<VideoThread>;

pub(super) async fn start(session: &mut Session, options: MirrorOptions) -> Result<StreamHandle> {
    start_with(session, options, VideoThread::spawn).await
}

async fn start_with(session: &mut Session, options: MirrorOptions, spawn: VideoSpawn) -> Result<StreamHandle> {
    clock::origin();
    if session.audio_uri.is_some() {
        return Err(Error::protocol("ya hay un flujo activo en esta sesión"));
    }
    if !session.info.supports_screen() {
        return Err(Error::protocol("el receptor no admite la duplicación de pantalla"));
    }
    if options.audio && !session.info.supports_screen_alac() {
        return Err(Error::protocol(
            "el receptor solo admite AAC-ELD para el audio de la pantalla, que no está soportado",
        ));
    }
    let secret = match (&session.secret, session.encrypted) {
        (Some(secret), true) => secret.clone(),
        _ => {
            return Err(Error::protocol(
                "el receptor usa el cifrado heredado (FairPlay), que todavía no está soportado al duplicar la pantalla",
            ));
        }
    };
    let audio_id = rand::random::<u64>() >> 1;
    let device_id = session.identity.device_id_string();
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let plan = Plan {
        options,
        peer: session.peer,
        setup: MirrorSetup {
            device_id: &device_id,
            session_uuid: &session_uuid,
            source_version: setup::MODERN_SOURCE_VERSION,
            timing_port: 0,
            name: &session.name,
        },
        audio_uri: format!("rtsp://{}:{}/{audio_id}", session.peer.ip(), session.peer.port()),
        session_uuid: session_uuid.clone(),
        secret,
        conn_arc: Arc::clone(&session.conn),
        spawn,
        info: session.info.clone(),
    };
    let conn_arc = Arc::clone(&session.conn);
    let mut conn = conn_arc.lock().await;
    conn.set_minimal_headers(true);
    let mut accepted = false;
    let uri = plan.audio_uri.clone();
    match run(plan, audio_id, &mut conn, &mut accepted).await {
        Ok(built) => {
            drop(conn);
            Ok(StreamHandle::mirror(
                Arc::clone(&session.conn),
                uri,
                session_uuid,
                built.pipeline,
                built.timing,
                built.events,
            ))
        }
        Err(e) => {
            if accepted {
                match timeout(TEARDOWN_TIMEOUT, conn.request("TEARDOWN", &uri, &[], None, &[])).await {
                    Ok(Ok(_)) => tracing::debug!("sesión de duplicación fallida liberada"),
                    other => tracing::debug!("no se pudo liberar la sesión fallida: {other:?}"),
                }
            }
            Err(e)
        }
    }
}

async fn setup_request(conn: &mut RtspConn, uri: &str, dict: Dictionary) -> Result<Response> {
    conn.request("SETUP", uri, &[], Some(BINARY_PLIST), &setup::to_binary(dict)?)
        .await
}

fn even(value: u32) -> u32 {
    (value & !1).max(2)
}

pub fn canvas(info: &ReceiverInfo) -> (u32, u32) {
    let forced = std::env::var("RUSTPLAY_TX_CANVAS").ok().and_then(|v| {
        let (w, h) = v.split_once(['x', 'X'])?;
        Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
    });
    let (w, h) = forced.unwrap_or_else(|| info.mirror_size());
    (even(w), even(h))
}

// Scales the canvas down so its height does not exceed `max_height`, keeping the aspect ratio.
pub fn cap_height((width, height): (u32, u32), max_height: Option<u32>) -> (u32, u32) {
    match max_height {
        Some(cap) if cap >= 2 && height > cap => {
            let scaled = (u64::from(width) * u64::from(cap) / u64::from(height)) as u32;
            (even(scaled.max(2)), even(cap))
        }
        _ => (width, height),
    }
}

// GET /info once the media session exists; a declined request keeps the earlier snapshot.
async fn refresh_info(conn: &mut RtspConn, info: &mut ReceiverInfo) -> Result<()> {
    match timeout(INFO_TIMEOUT, conn.request("GET", "/info", &[], None, &[])).await {
        Err(_) => Err(Error::Timeout),
        Ok(Err(e)) => Err(e),
        Ok(Ok(response)) if response.status == 200 => {
            if let Ok(Value::Dictionary(dict)) = Value::from_reader(std::io::Cursor::new(&response.body[..])) {
                info.adopt_session_info(&dict);
            }
            Ok(())
        }
        Ok(Ok(response)) => {
            tracing::debug!("GET /info rechazado tras crear la sesión: {}", response.status);
            Ok(())
        }
    }
}

struct Progress {
    timing: TimingService,
    events: Option<EventChannel>,
    event_port_tried: u16,
    skip_record: bool,
}

impl Progress {
    async fn observe(&mut self, plan: &Plan<'_>, reply: &MirrorReply) {
        if reply.timing_port > 0 {
            self.timing.probe(SocketAddr::new(plan.peer.ip(), reply.timing_port));
        }
        if self.events.is_none() && reply.event_port > 0 && self.event_port_tried != reply.event_port {
            self.event_port_tried = reply.event_port;
            let target = SocketAddr::new(plan.peer.ip(), reply.event_port);
            match EventChannel::connect(target, Some(plan.secret.as_slice())).await {
                Ok(channel) => self.events = Some(channel),
                Err(e) => tracing::warn!("no se pudo conectar el canal de eventos del receptor: {e}"),
            }
        }
    }
}

struct VideoSlots {
    pool_rx: Option<mpsc::Receiver<Vec<u8>>>,
    units_tx: Option<mpsc::Sender<crate::video::Unit>>,
    shared: Arc<Shared>,
    thread: Option<VideoThread>,
    size: (u32, u32),
}

impl VideoSlots {
    async fn prepare(&mut self, plan: &Plan<'_>) -> Result<()> {
        let (Some(pool_rx), Some(units_tx)) = (self.pool_rx.take(), self.units_tx.take()) else {
            return Err(Error::protocol("el vídeo ya estaba preparado"));
        };
        let (width, height) = cap_height(canvas(&plan.info), plan.options.max_height);
        tracing::info!("Preparando la captura y el codificador de vídeo ({width}x{height})…");
        let params = VideoParams {
            monitor: plan.options.monitor,
            window: plan.options.window,
            width,
            height,
            fps: plan.options.fps.max(1),
            bitrate: plan.options.bitrate,
            cursor: plan.options.cursor,
        };
        let shared = Arc::clone(&self.shared);
        let spawn = plan.spawn;
        let thread = tokio::task::spawn_blocking(move || spawn(params, shared, pool_rx, units_tx))
            .await
            .map_err(|e| Error::protocol(e.to_string()))??;
        tracing::info!("Codificador: {}", thread.encoder);
        self.thread = Some(thread);
        self.size = (width, height);
        Ok(())
    }
}

async fn run(mut plan: Plan<'_>, audio_id: u64, conn: &mut RtspConn, accepted: &mut bool) -> Result<Built> {
    let peer = plan.peer;
    let mut sockets = clock::allocate_consecutive_udp(3, None, peer.ip())?.into_iter();
    let mut next_socket = || sockets.next().ok_or_else(|| Error::protocol("faltan puertos UDP"));
    let timing_socket = next_socket()?;
    let control_socket = clock::into_tokio(next_socket()?)?;
    let data_socket = clock::into_tokio(next_socket()?)?;
    let timing = TimingService::start(timing_socket)?;
    let control_port = control_socket.local_addr()?.port();
    plan.setup.timing_port = timing.port();
    tracing::debug!(
        "puertos UDP consecutivos: tiempo={} control={} datos={}",
        timing.port(),
        control_port,
        data_socket.local_addr()?.port()
    );
    let mut progress = Progress { timing, events: None, event_port_tried: 0, skip_record: false };

    let audio_key: [u8; 32] = rand::random();
    let pending = if plan.options.audio { Some(PendingCapture::start()?) } else { None };
    let silence = if pending.is_some() { Some(audio::encode_silence()?) } else { None };

    let video_id = rand::random::<u64>() >> 1;
    let (shared, closed) = Shared::new();
    let (pool_tx, pool_rx) = mpsc::channel::<Vec<u8>>(POOL_BUFFERS);
    for _ in 0..POOL_BUFFERS {
        let _ = pool_tx.try_send(Vec::with_capacity(256 * 1024));
    }
    let (units_tx, units_rx) = mpsc::channel(QUEUE_DEPTH);
    let mut video = VideoSlots {
        pool_rx: Some(pool_rx),
        units_tx: Some(units_tx),
        shared: Arc::clone(&shared),
        thread: None,
        size: (0, 0),
    };

    tracing::info!("SETUP de la sesión de duplicación…");
    let control = setup_request(conn, &plan.audio_uri, mirror_setup::control_plist(&plan.setup, None)).await?;
    let mut session_first = true;
    if control.status == 200 {
        *accepted = true;
        let reply = mirror_setup::parse_reply(&control.body)?;
        if let Some(dict) = &reply.info {
            plan.info.adopt_session_info(dict);
            let keys: Vec<&str> = dict.keys().map(String::as_str).collect();
            tracing::debug!(?keys, formats = ?dict.get("supportedFormats"), "información del receptor en el SETUP de control");
        }
        tracing::info!(
            "Formatos de audio de pantalla del receptor: {}",
            plan.info.screen_stream_formats.map_or_else(|| "no anunciados".to_owned(), |m| format!("{m:#x}"))
        );
        if plan.options.audio && !plan.info.supports_screen_alac() {
            tracing::warn!("el receptor no anuncia ALAC para el audio de pantalla; el audio probablemente no sonará");
        }
        if plan.info.display.is_none() {
            refresh_info(conn, &mut plan.info).await?;
        }
        video.prepare(&plan).await?;
        progress.observe(&plan, &reply).await;
        progress.skip_record = reply.skip_record;
    } else if mirror_setup::order_rejected(control.status) {
        tracing::debug!(
            "el receptor rechazó el SETUP de sesión ({}); se usará el orden con el flujo primero",
            control.status
        );
        session_first = false;
    } else {
        ensure_ok("SETUP de sesión", &control)?;
    }

    let requested = std::env::var("RUSTPLAY_TX_AUDIO_LATENCY_MS").ok().or_else(|| plan.options.audio_latency_ms.map(|ms| ms.to_string()));
    let audio_latency = audio_latency_samples(requested.as_deref());
    tracing::info!(
        "SETUP del audio de la pantalla (ALAC, 44,1 kHz, {} ms de latencia)…",
        u64::from(audio_latency) * 1000 / 44_100
    );
    let mut layout = if info::uses_stream_connections(&plan.info) {
        Layout::StreamConnections
    } else {
        Layout::ControlPort
    };
    let audio_request = |plan: &Plan, layout: Layout| {
        let stream =
            mirror_setup::screen_audio_stream(audio_id, audio_latency, Some(&audio_key), control_port, layout);
        if session_first {
            mirror_setup::streams_only(stream)
        } else {
            mirror_setup::streams_with_session(&plan.setup, stream, None)
        }
    };
    let mut response = setup_request(conn, &plan.audio_uri, audio_request(&plan, layout)).await?;
    if mirror_setup::shape_rejected(response.status) {
        layout = layout.other();
        tracing::debug!("SETUP de audio rechazado; se reintenta con el descriptor alternativo ({layout:?})");
        response = setup_request(conn, &plan.audio_uri, audio_request(&plan, layout)).await?;
    }
    ensure_ok("SETUP del audio", &response)?;
    *accepted = true;
    let audio_reply = mirror_setup::parse_reply(&response.body)?;
    progress.observe(&plan, &audio_reply).await;
    if !session_first {
        progress.skip_record = audio_reply.skip_record;
        refresh_info(conn, &mut plan.info).await?;
        video.prepare(&plan).await?;
    }
    let audio_ports: Option<StreamPorts> = audio_reply.audio.filter(|p| p.data > 0 && p.control > 0);
    tracing::debug!(?audio_ports, "puertos de audio del receptor");

    tracing::info!("SETUP del vídeo…");
    let (shk_source, shiv_source) = control_keys_from(&plan.secret)?;
    let video_uri = format!("rtsp://{}:{}/{video_id}", peer.ip(), peer.port());
    let video_dict = mirror_setup::streams_only(mirror_setup::video_stream(
        video_id,
        VIDEO_LATENCY.as_millis() as u64,
        Some((&shk_source[..16], &shiv_source[..16])),
    ));
    let response = setup_request(conn, &video_uri, video_dict).await?;
    ensure_ok("SETUP del vídeo", &response)?;
    let video_reply = mirror_setup::parse_reply(&response.body)?;
    progress.observe(&plan, &video_reply).await;
    if video_reply.video_data_port == 0 {
        return Err(Error::protocol("el receptor no indicó el puerto de datos de vídeo"));
    }

    let data_addr = SocketAddr::new(peer.ip(), video_reply.video_data_port);
    let socket = if peer.is_ipv6() { TcpSocket::new_v6()? } else { TcpSocket::new_v4()? };
    socket.set_nodelay(true)?;
    socket.set_send_buffer_size(64 * 1024)?;
    let stream = timeout(DATA_CONNECT_TIMEOUT, socket.connect(data_addr))
        .await
        .map_err(|_| Error::Timeout)??;
    tracing::debug!("canal de datos de vídeo conectado: {data_addr}");

    if progress.skip_record {
        tracing::debug!("el receptor indicó skipRecord; no se envía RECORD");
    } else {
        let headers = [
            ("Session", plan.session_uuid.as_str()),
            ("Range", "npt=0-"),
            ("RTP-Info", "seq=0;rtptime=0"),
        ];
        let record = conn.request("RECORD", &plan.audio_uri, &headers, None, &[]).await?;
        ensure_ok("RECORD", &record)?;
    }
    if let (true, Some(db)) = (plan.options.audio, plan.options.volume_db) {
        let body = format!("volume: {:.6}\r\n", db.clamp(-144.0, 0.0));
        let volume = conn
            .request("SET_PARAMETER", &plan.audio_uri, &[], Some(PARAMETERS), body.as_bytes())
            .await?;
        if volume.status != 200 {
            tracing::debug!("SET_PARAMETER de volumen: {}", volume.status);
        }
    }

    let data_key = data_stream_key(&plan.secret, video_id, true)?;
    let (reader, writer) = stream.into_split();
    let sender = DataSender::new(writer, Some(FrameCipher::new(&data_key)), VIDEO_LATENCY);

    let audio = match (pending, silence, audio_ports) {
        (Some(pending), Some(silence), Some(ports)) => Some(AudioLaunch {
            pending,
            params: audio_params(&plan, &audio_key, control_socket, data_socket, ports, silence, audio_latency)?,
        }),
        (Some(_), _, None) => {
            tracing::warn!("el receptor no ha indicado puertos de audio; se duplica solo el vídeo");
            None
        }
        _ => None,
    };

    let thread = video
        .thread
        .take()
        .ok_or_else(|| Error::protocol("la captura de vídeo no se preparó"))?;
    let pipeline = MirrorPipeline::launch(MirrorParams {
        reader,
        sender,
        video: thread,
        shared,
        closed,
        units: units_rx,
        pool: pool_tx,
        conn: Arc::clone(&plan.conn_arc),
        audio,
        size: video.size,
    });
    Ok(Built { pipeline, timing: progress.timing, events: progress.events })
}

// RUSTPLAY_TX_AUDIO_LATENCY_MS overrides the playout lead of the mirrored audio.
pub fn audio_latency_samples(requested_ms: Option<&str>) -> u32 {
    requested_ms
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(AUDIO_LATENCY_SAMPLES, |ms| {
            crate::pacing::latency_samples(ms.clamp(AUDIO_LATENCY_RANGE_MS.0, AUDIO_LATENCY_RANGE_MS.1))
        })
}

fn audio_params(
    plan: &Plan<'_>,
    key: &[u8; 32],
    control_socket: UdpSocket,
    data_socket: UdpSocket,
    ports: StreamPorts,
    silence: Vec<u8>,
    latency_samples: u32,
) -> Result<PipelineParams> {
    let ip = plan.peer.ip();
    Ok(PipelineParams {
        data_socket,
        control_socket,
        data_peer: SocketAddr::new(ip, ports.data),
        control_peer: SocketAddr::new(ip, ports.control),
        latency_samples,
        builder: PacketBuilder::new(1, rand::random::<u32>(), 0, Box::new(ChaChaProtection::new(key, false)?)),
        silence,
        redundancy: true,
        sync_clock: SyncClock::Boot,
        sync_sequence: 4,
        feedback: false,
    })
}

#[cfg(test)]
mod tests;
