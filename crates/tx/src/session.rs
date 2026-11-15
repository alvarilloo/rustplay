use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use rustplay_core::identity::{Identity, PairingRecord, PairingStore};
use rustplay_core::mdns::DiscoveredDevice;
use rustplay_core::ntp::NtpResponder;
use rustplay_core::{Error, Result};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::audio::{self, PendingCapture, Pipeline, PipelineParams};
use crate::conn::{RtspConn, ensure_ok};
use crate::event::EventChannel;
use crate::info::{self, Credential, ReceiverInfo, SecurityMode};
use crate::clock::TimingService;
use crate::mirror::MirrorPipeline;
use crate::pacing;
use crate::pairing::{self, FairPlayKeys, Method, PairingContext};
use crate::rtp::{AesCbcProtection, NoProtection, PacketBuilder, Protector};
use crate::seal::ChaChaProtection;
use crate::setup::{self, Layout, Profile, SessionSetup, StreamSetup};
use crate::{AudioOptions, MirrorOptions};

const BINARY_PLIST: &str = "application/x-apple-binary-plist";
const PARAMETERS: &str = "text/parameters";

pub struct Session {
    conn: Arc<Mutex<RtspConn>>,
    info: ReceiverInfo,
    identity: Identity,
    peer: SocketAddr,
    name: String,
    encrypted: bool,
    secret: Option<Vec<u8>>,
    fairplay: Option<FairPlayKeys>,
    session_uuid: String,
    audio_uri: Option<String>,
}

mod mirror_flow;

pub struct StreamHandle {
    conn: Arc<Mutex<RtspConn>>,
    uri: String,
    session_uuid: String,
    pipeline: Option<Pipeline>,
    _ntp: Option<NtpResponder>,
    _timing: Option<TimingService>,
    mirror: Option<MirrorPipeline>,
    _events: Option<EventChannel>,
}

pub fn pick_address(device: &DiscoveredDevice) -> Result<IpAddr> {
    device
        .addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(device.addrs.first())
        .copied()
        .ok_or_else(|| Error::protocol("el dispositivo no ha publicado ninguna dirección IP"))
}

pub fn device_key(device: &DiscoveredDevice, info: &ReceiverInfo) -> String {
    let raw = device
        .device_id()
        .or_else(|| (!info.device_id.is_empty()).then_some(info.device_id.as_str()))
        .or_else(|| device.instance.split_once('@').map(|(mac, _)| mac))
        .unwrap_or(device.instance.as_str());
    raw.chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn sender_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| String::from("PC con Windows"))
}

fn setup_shape_rejected(status: u16) -> bool {
    matches!(status, 400 | 406 | 415 | 455 | 500 | 501)
}

pub async fn fetch_info(conn: &mut RtspConn, txt: &HashMap<String, String>) -> Result<ReceiverInfo> {
    let response = conn.call("GET /info", "GET", "/info", &[], None, &[]).await?;
    ReceiverInfo::parse(&response.body, txt)
}

impl Session {
    pub async fn connect<F, Fut>(device: &DiscoveredDevice, store: &mut PairingStore, pin: F) -> Result<Self>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let peer = SocketAddr::new(pick_address(device)?, device.port);
        Self::connect_to(peer, device, store, pin, None).await
    }

    #[doc(hidden)]
    pub async fn connect_to<F, Fut>(
        peer: SocketAddr,
        device: &DiscoveredDevice,
        store: &mut PairingStore,
        pin: F,
        plan_override: Option<Vec<Method>>,
    ) -> Result<Self>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let label = device.display_name().to_owned();
        tracing::info!("Conectando con «{label}» ({peer})…");
        let identity = Identity::load_or_create()?;
        let mut conn = RtspConn::connect(peer).await?;
        let info = fetch_info(&mut conn, &device.txt).await?;
        tracing::info!(
            "Receptor «{}»: {} (id {})",
            if info.name.is_empty() { &label } else { &info.name },
            info.describe(),
            info.device_id
        );
        if info.requires_password() && info.credential() == Credential::Password {
            return Err(Error::Pairing(
                "el receptor requiere una contraseña de reproducción, que todavía no está soportada".into(),
            ));
        }
        if info.ptp_advertised() {
            tracing::debug!("el receptor anuncia PTP; se usará NTP para el reloj");
        }
        let key = device_key(device, &info);
        let client_name = sender_name();
        let ctx = PairingContext {
            identity: &identity,
            info: &info,
            client_name: &client_name,
        };
        let stored = store.find(&key).cloned();
        let mut queue: VecDeque<Method> = plan_override
            .unwrap_or_else(|| pairing::plan(&info, stored.is_some()))
            .into();
        let mut pin = Some(pin);
        let mut pin_tried = false;
        let mut first = true;
        let mut last_error: Option<Error> = None;
        while let Some(method) = queue.pop_front() {
            if !first {
                conn = RtspConn::connect(peer).await?;
            }
            first = false;
            let outcome = run_method(method, &mut conn, &ctx, stored.as_ref(), store, &key, &mut pin).await;
            match outcome {
                Ok(paired) => {
                    let encrypted = conn.is_encrypted();
                    tracing::info!("Emparejado con «{label}»");
                    let fairplay = if encrypted {
                        None
                    } else if info.supports_fairplay() {
                        tracing::info!("Negociando FairPlay…");
                        Some(pairing::fairplay_setup(&mut conn, paired.mixed_secret.as_ref()).await?)
                    } else {
                        None
                    };
                    return Ok(Self {
                        conn: Arc::new(Mutex::new(conn)),
                        info,
                        identity,
                        peer,
                        name: client_name,
                        encrypted,
                        secret: paired.secret,
                        fairplay,
                        session_uuid: uuid::Uuid::new_v4().to_string().to_uppercase(),
                        audio_uri: None,
                    });
                }
                Err(e) => {
                    let pin_needed = matches!(&e, Error::Protocol(m) if m.contains("470"));
                    tracing::warn!("el método de emparejamiento {method:?} falló: {e}");
                    if pin_needed && !pin_tried && method != Method::Pin {
                        queue.push_front(Method::Pin);
                    }
                    pin_tried |= method == Method::Pin;
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::Pairing("no hay ningún método de emparejamiento aplicable".into())))
    }

    pub fn info(&self) -> &ReceiverInfo {
        &self.info
    }

    pub async fn start_audio(&mut self, options: AudioOptions) -> Result<StreamHandle> {
        if self.audio_uri.is_some() {
            return Err(Error::protocol("ya hay un flujo de audio activo en esta sesión"));
        }
        if !self.info.supports_alac() {
            return Err(Error::protocol("el receptor no admite audio ALAC"));
        }
        let profile = Profile::from_env();
        tracing::info!("Perfil de emisión: {profile:?}");
        let silence = audio::encode_silence()?;
        let capture = PendingCapture::start()?;

        let local = rustplay_core::net::listen_addr(self.peer.ip());
        let mut ntp = NtpResponder::bind(self.peer.ip()).await?;
        let control_socket = UdpSocket::bind((local, 0)).await?;
        let data_socket = UdpSocket::bind((local, 0)).await?;
        let control_port = control_socket.local_addr()?.port();

        let local_ip = self.conn.lock().await.local_addr()?.ip();
        let session_id = rand::random::<u32>();
        let (stream_id, uri) = match profile {
            Profile::OwnTone => (u64::from(session_id), session_url(local_ip, session_id)),
            Profile::Doubletake => {
                let id = rand::random::<u64>() >> 1;
                (id, format!("rtsp://{}:{}/{id}", self.peer.ip(), self.peer.port()))
            }
        };
        let latency_samples = pacing::latency_samples(options.latency_ms);
        let security = info::audio_security(self.encrypted);
        let shk = (security == SecurityMode::ChaCha).then(|| audio_key_for(profile, self.secret.as_deref()));
        let protector: Box<dyn Protector> = match (&shk, &self.fairplay) {
            (Some(key), _) => Box::new(ChaChaProtection::new(key, profile == Profile::OwnTone)?),
            (None, Some(fp)) => Box::new(AesCbcProtection::new(fp.audio_key, fp.eiv)),
            (None, None) => {
                tracing::warn!("el flujo de audio se enviará sin cifrar");
                Box::new(NoProtection)
            }
        };

        let source_version = match security {
            SecurityMode::ChaCha => setup::MODERN_SOURCE_VERSION,
            SecurityMode::Aes => setup::LEGACY_SOURCE_VERSION,
        };
        let device_id = self.identity.device_id_string();
        let session = SessionSetup {
            device_id: &device_id,
            session_uuid: &self.session_uuid,
            source_version,
            timing_port: ntp.local_port(),
            name: &self.name,
            fairplay: self.fairplay.as_ref().map(|f| (&f.ekey, &f.eiv)),
        };
        let layout = if info::uses_stream_connections(&self.info) {
            Layout::StreamConnections
        } else {
            Layout::ControlPort
        };
        let (sequence, timestamp, ssrc) = (rand::random::<u16>(), rand::random::<u32>(), rand::random::<u32>());
        let event_secret = if self.encrypted { self.secret.as_deref() } else { None };
        let mut conn = self.conn.lock().await;
        let negotiated = negotiate(
            &mut conn,
            Negotiation {
                profile,
                uri: &uri,
                session: &session,
                stream: StreamSetup {
                    stream_connection_id: stream_id,
                    control_port,
                    latency_samples,
                    shk: shk.as_ref(),
                    layout,
                },
                peer: self.peer,
                event_secret,
                ntp: &mut ntp,
                ptp_hint: self.info.ptp_advertised(),
                rtp_info: format!("seq={sequence};rtptime={timestamp}"),
                volume_db: options.volume_db,
            },
        )
        .await?;
        drop(conn);
        let Negotiated { reply, events } = negotiated;

        let peer_ip = self.peer.ip();
        let pipeline = capture.launch(
            PipelineParams {
                data_socket,
                control_socket,
                data_peer: SocketAddr::new(peer_ip, reply.data_port),
                control_peer: SocketAddr::new(peer_ip, reply.control_port),
                latency_samples,
                builder: PacketBuilder::new(sequence, timestamp, ssrc, protector),
                silence,
                redundancy: false,
                sync_clock: audio::SyncClock::Wall,
                sync_sequence: 7,
                feedback: true,
            },
            Arc::clone(&self.conn),
        );
        self.audio_uri = Some(uri.clone());
        tracing::info!("Transmitiendo audio ({} ms de latencia)", options.latency_ms);
        Ok(StreamHandle {
            conn: Arc::clone(&self.conn),
            uri,
            session_uuid: self.session_uuid.clone(),
            pipeline: Some(pipeline),
            _ntp: Some(ntp),
            _timing: None,
            mirror: None,
            _events: events,
        })
    }

    pub async fn start_mirror(&mut self, options: MirrorOptions) -> Result<StreamHandle> {
        mirror_flow::start(self, options).await
    }

    pub async fn set_volume(&mut self, db: f32) -> Result<()> {
        let uri = self
            .audio_uri
            .clone()
            .ok_or_else(|| Error::protocol("inicia el audio antes de ajustar el volumen"))?;
        let body = format!("volume: {:.6}\r\n", db.clamp(-144.0, 0.0));
        let response = self
            .conn
            .lock()
            .await
            .request(
                "SET_PARAMETER",
                &uri,
                &[("Session", self.session_uuid.as_str())],
                Some(PARAMETERS),
                body.as_bytes(),
            )
            .await?;
        ensure_ok("SET_PARAMETER (volumen)", &response)
    }

    pub async fn close(self) -> Result<()> {
        if let Some(uri) = &self.audio_uri {
            let mut conn = self.conn.lock().await;
            let result = conn
                .request("TEARDOWN", uri, &[("Session", self.session_uuid.as_str())], None, &[])
                .await;
            if let Err(e) = result {
                tracing::debug!("TEARDOWN de la sesión: {e}");
            }
        }
        Ok(())
    }
}

impl StreamHandle {
    pub(crate) fn mirror(
        conn: Arc<Mutex<RtspConn>>,
        uri: String,
        session_uuid: String,
        pipeline: MirrorPipeline,
        timing: TimingService,
        events: Option<EventChannel>,
    ) -> Self {
        Self {
            conn,
            uri,
            session_uuid,
            pipeline: None,
            _ntp: None,
            _timing: Some(timing),
            mirror: Some(pipeline),
            _events: events,
        }
    }

    pub fn mirror_stats(&self) -> Option<&crate::video::MirrorStats> {
        self.mirror.as_ref().map(MirrorPipeline::stats)
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.mirror.as_ref().map(|m| m.size)
    }

    pub fn encoder_name(&self) -> Option<&str> {
        self.mirror.as_ref().map(|m| m.encoder.as_str())
    }

    pub fn failure(&self) -> Option<String> {
        self.mirror.as_ref().and_then(MirrorPipeline::failure)
    }

    // Resolves when the receiver stops responding to keep-alive requests.
    pub async fn closed(&self) {
        if let Some(mirror) = &self.mirror {
            return mirror.closed().await;
        }
        match &self.pipeline {
            Some(pipeline) => pipeline.closed().await,
            None => std::future::pending().await,
        }
    }

    pub fn stats(&self) -> Option<&Arc<audio::Stats>> {
        self.pipeline.as_ref().map(|p| &p.stats)
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(mirror) = self.mirror.take() {
            mirror.stop().await;
            let result = self.conn.lock().await.request("TEARDOWN", &self.uri, &[], None, &[]).await;
            if let Err(e) = result {
                tracing::debug!("TEARDOWN de la duplicación: {e}");
            }
            return Ok(());
        }
        if let Some(pipeline) = self.pipeline.take() {
            pipeline.stop().await;
        }
        let body = setup::to_binary(setup::teardown_plist())?;
        let result = self
            .conn
            .lock()
            .await
            .request(
                "TEARDOWN",
                &self.uri,
                &[("Session", self.session_uuid.as_str())],
                Some(BINARY_PLIST),
                &body,
            )
            .await;
        if let Err(e) = result {
            tracing::debug!("TEARDOWN del flujo: {e}");
        }
        Ok(())
    }
}

fn session_url(local: IpAddr, session_id: u32) -> String {
    match local {
        IpAddr::V4(ip) => format!("rtsp://{ip}/{session_id}"),
        IpAddr::V6(ip) => format!("rtsp://[{ip}]/{session_id}"),
    }
}

// OwnTone keys the audio with the first 32 bytes of the pairing secret; without one (or for the
// doubletake profile) a random key is used.
pub fn audio_key_for(profile: Profile, secret: Option<&[u8]>) -> [u8; 32] {
    if profile == Profile::OwnTone
        && let Some(secret) = secret.filter(|s| s.len() >= 32)
    {
        let mut key = [0u8; 32];
        key.copy_from_slice(&secret[..32]);
        return key;
    }
    let mut key = [0u8; 32];
    rand::fill(&mut key[..]);
    key
}

async fn connect_events(peer: SocketAddr, port: u16, secret: Option<&[u8]>) -> Option<EventChannel> {
    if port == 0 {
        return None;
    }
    match EventChannel::connect(SocketAddr::new(peer.ip(), port), secret).await {
        Ok(channel) => Some(channel),
        Err(e) => {
            tracing::warn!("no se pudo conectar el canal de eventos del receptor: {e}");
            None
        }
    }
}

struct Negotiation<'a> {
    profile: Profile,
    uri: &'a str,
    session: &'a SessionSetup<'a>,
    stream: StreamSetup<'a>,
    peer: SocketAddr,
    event_secret: Option<&'a [u8]>,
    ntp: &'a mut NtpResponder,
    ptp_hint: bool,
    rtp_info: String,
    volume_db: Option<f32>,
}

struct Negotiated {
    reply: setup::SetupReply,
    events: Option<EventChannel>,
}

async fn send_stream_setup(
    conn: &mut RtspConn,
    n: &Negotiation<'_>,
    layout: Layout,
    with_session_fields: bool,
) -> Result<rustplay_core::rtsp::Response> {
    let stream = StreamSetup { layout, ..n.stream };
    let mut root = setup::stream_plist(&stream, n.profile);
    if with_session_fields {
        for (key, value) in setup::session_plist(n.session, n.profile) {
            root.insert(key, value);
        }
    }
    conn.request("SETUP", n.uri, &[], Some(BINARY_PLIST), &setup::to_binary(root)?)
        .await
}

async fn send_record(conn: &mut RtspConn, n: &Negotiation<'_>, events: Option<&EventChannel>) -> Result<()> {
    tracing::info!("RECORD…");
    // OwnTone sends RECORD with no extra headers; the RTP start is carried by the data and sync packets.
    let session_header = [("Session", n.session.session_uuid)];
    let doubletake_headers = [
        ("Session", n.session.session_uuid),
        ("Range", "npt=0-"),
        ("RTP-Info", n.rtp_info.as_str()),
    ];
    let headers: &[(&str, &str)] = match n.profile {
        Profile::OwnTone => &[],
        Profile::Doubletake => &doubletake_headers,
    };
    let _ = session_header;
    match conn.request("RECORD", n.uri, headers, None, &[]).await {
        Ok(record) => {
            ensure_ok("RECORD", &record)?;
            if let Some(latency) = record.headers.get("Audio-Latency") {
                tracing::debug!("latencia de audio del receptor: {latency} muestras");
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                "RECORD sin respuesta ({e}). Causas probables: el Firewall de Windows bloquea el UDP entrante al puerto de tiempo {} (la TV no puede sincronizar el reloj), o el canal de eventos no ha recibido nada (eventos recibidos: {}). Con -v se registran las peticiones de tiempo NTP recibidas.",
                n.ntp.local_port(),
                events.map_or(0, EventChannel::received)
            );
            Err(e)
        }
    }
}

// OwnTone order: SETUP (session), RECORD, SETUP (stream), SET_PARAMETER volume.
// doubletake order: SETUP (session), SETUP (stream), RECORD.
async fn negotiate(conn: &mut RtspConn, n: Negotiation<'_>) -> Result<Negotiated> {
    tracing::info!("SETUP de la sesión de audio…");
    let session_body = setup::to_binary(setup::session_plist(n.session, n.profile))?;
    let response = conn
        .request("SETUP", n.uri, &[], Some(BINARY_PLIST), &session_body)
        .await?;
    let mut session_reply = setup::SetupReply::default();
    let mut session_first = true;
    let mut events: Option<EventChannel> = None;
    if response.status == 200 {
        session_reply = setup::parse_setup_reply(&response.body)?;
        if session_reply.timing_port != 0 {
            n.ntp.probe(SocketAddr::new(n.peer.ip(), session_reply.timing_port));
        }
        events = connect_events(n.peer, session_reply.event_port, n.event_secret).await;
    } else if n.profile == Profile::Doubletake && setup_shape_rejected(response.status) {
        tracing::debug!(
            "el receptor rechazó el SETUP de sesión ({}); se combinará con el del flujo",
            response.status
        );
        session_first = false;
    } else {
        ensure_ok("SETUP de sesión", &response)?;
    }

    if n.profile == Profile::OwnTone {
        send_record(conn, &n, events.as_ref()).await?;
    }
    tracing::info!("SETUP del flujo de audio (ALAC, 44,1 kHz)…");
    let mut layout = n.stream.layout;
    let mut response = send_stream_setup(conn, &n, layout, !session_first).await?;
    if n.profile == Profile::Doubletake && setup_shape_rejected(response.status) {
        layout = layout.other();
        tracing::debug!("SETUP rechazado; se reintenta con el descriptor alternativo ({layout:?})");
        response = send_stream_setup(conn, &n, layout, !session_first).await?;
    }
    if response.status != 200 && n.ptp_hint {
        tracing::warn!(
            "el receptor anuncia PTP y ha rechazado el SETUP; si requiere PTP no es compatible con este emisor (solo NTP)"
        );
    }
    ensure_ok("SETUP del flujo de audio", &response)?;
    let reply = setup::parse_setup_reply(&response.body)?;
    if reply.timing_port != 0 {
        n.ntp.probe(SocketAddr::new(n.peer.ip(), reply.timing_port));
    }
    if events.is_none() {
        events = connect_events(n.peer, reply.event_port, n.event_secret).await;
    }
    if reply.data_port == 0 {
        return Err(Error::protocol("el receptor no indicó el puerto de datos de audio"));
    }
    tracing::debug!(
        data = reply.data_port,
        control = reply.control_port,
        timing = session_reply.timing_port.max(reply.timing_port),
        event = session_reply.event_port.max(reply.event_port),
        "puertos del receptor"
    );
    if n.profile == Profile::Doubletake {
        send_record(conn, &n, events.as_ref()).await?;
    }
    if let Some(db) = n.volume_db {
        let body = format!("volume: {:.6}\r\n", db.clamp(-144.0, 0.0));
        let session_header = [("Session", n.session.session_uuid)];
        let headers: &[(&str, &str)] = if n.profile == Profile::Doubletake { &session_header } else { &[] };
        let response = conn
            .request("SET_PARAMETER", n.uri, headers, Some(PARAMETERS), body.as_bytes())
            .await?;
        ensure_ok("SET_PARAMETER (volumen)", &response)?;
    }
    Ok(Negotiated { reply, events })
}

struct Paired {
    mixed_secret: Option<[u8; 32]>,
    secret: Option<Vec<u8>>,
}

async fn run_method<F, Fut>(
    method: Method,
    conn: &mut RtspConn,
    ctx: &PairingContext<'_>,
    stored: Option<&PairingRecord>,
    store: &mut PairingStore,
    key: &str,
    pin: &mut Option<F>,
) -> Result<Paired>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    match method {
        Method::Stored => {
            let record = stored.ok_or_else(|| Error::Pairing("no hay credenciales guardadas".into()))?;
            tracing::info!("Emparejando con las credenciales guardadas…");
            let secret = pairing::pair_stored(conn, ctx, record).await?;
            Ok(Paired { mixed_secret: None, secret: Some(secret.to_vec()) })
        }
        Method::Transient => {
            tracing::info!("Emparejando (sin PIN)…");
            let secret = pairing::pair_transient(conn, ctx, false).await?;
            Ok(Paired { mixed_secret: None, secret: Some(secret) })
        }
        Method::TransientEmptyPassword => {
            tracing::info!("Emparejando (sin PIN, contraseña vacía)…");
            let secret = pairing::pair_transient(conn, ctx, true).await?;
            Ok(Paired { mixed_secret: None, secret: Some(secret) })
        }
        Method::Pin => {
            tracing::info!("PIN requerido: el receptor debería mostrarlo en pantalla");
            pairing::start_pin_display(conn, ctx).await?;
            let ask = pin
                .take()
                .ok_or_else(|| Error::Pairing("el PIN introducido no fue válido".into()))?;
            let code = ask().await?;
            let outcome = pairing::pair_pin(conn, ctx, code.trim()).await?;
            if let Some(record) = outcome.record {
                let saved = store.upsert(PairingRecord {
                    peer_id: key.to_owned(),
                    peer_public_key: record.peer_public_key,
                });
                match saved {
                    Ok(()) => tracing::info!("Credenciales guardadas para próximas conexiones"),
                    Err(e) => tracing::warn!("no se pudieron guardar las credenciales: {e}"),
                }
            }
            Ok(Paired { mixed_secret: None, secret: Some(outcome.shared_secret.to_vec()) })
        }
        Method::LegacyRaw => {
            tracing::info!("Emparejando (protocolo clásico)…");
            let mixed_secret = pairing::pair_legacy_raw(conn, ctx).await?;
            Ok(Paired { mixed_secret, secret: None })
        }
        Method::FairPlayOnly => Ok(Paired { mixed_secret: None, secret: None }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use rustplay_core::rtsp::{Response, ServerCodec};
    use bytes::BytesMut;
    use plist::{Dictionary, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::codec::{Decoder, Encoder};

    use super::*;

    type Entry = (String, Vec<(String, String)>, Vec<u8>);
    type Log = Arc<StdMutex<Vec<Entry>>>;

    fn bplist(entries: Vec<(&str, Value)>) -> Vec<u8> {
        let mut d = Dictionary::new();
        for (k, v) in entries {
            d.insert(k.to_owned(), v);
        }
        setup::to_binary(d).unwrap()
    }

    async fn fake_receiver(log: Log) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut codec = ServerCodec;
            let mut inbound = BytesMut::new();
            let mut setups = 0;
            loop {
                while let Some(request) = codec.decode(&mut inbound).unwrap() {
                    let headers = request.headers.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
                    log.lock()
                        .unwrap()
                        .push((request.method.clone(), headers, request.body.to_vec()));
                    let mut response = Response::ok(request.protocol);
                    response.headers.set("CSeq", request.cseq().unwrap_or("0"));
                    if request.method == "SETUP" {
                        setups += 1;
                        let body = if request.body.windows(7).any(|w| w == b"streams") || setups == 2 {
                            let mut stream = Dictionary::new();
                            stream.insert("dataPort".into(), Value::Integer(7001u64.into()));
                            stream.insert("controlPort".into(), Value::Integer(7002u64.into()));
                            bplist(vec![("streams", Value::Array(vec![Value::Dictionary(stream)]))])
                        } else {
                            bplist(vec![("eventPort", Value::Integer(0u64.into()))])
                        };
                        response = response.with_body("application/x-apple-binary-plist", body.into());
                    }
                    let mut out = BytesMut::new();
                    codec.encode(response, &mut out).unwrap();
                    stream.write_all(&out).await.unwrap();
                }
                if stream.read_buf(&mut inbound).await.unwrap() == 0 {
                    return;
                }
            }
        });
        addr
    }

    async fn run(profile: Profile, volume: Option<f32>) -> Vec<Entry> {
        let log: Log = Arc::default();
        let addr = fake_receiver(Arc::clone(&log)).await;
        let mut conn = RtspConn::connect(addr).await.unwrap();
        let mut ntp = NtpResponder::bind(addr.ip()).await.unwrap();
        let session = SessionSetup {
            device_id: "AA:BB:CC:DD:EE:FF",
            session_uuid: "UUID",
            source_version: setup::MODERN_SOURCE_VERSION,
            timing_port: ntp.local_port(),
            name: "PC",
            fairplay: None,
        };
        let shk = [7u8; 32];
        let negotiated = negotiate(
            &mut conn,
            Negotiation {
                profile,
                uri: "rtsp://127.0.0.1/123",
                session: &session,
                stream: StreamSetup {
                    stream_connection_id: 123,
                    control_port: 6001,
                    latency_samples: 13_230,
                    shk: Some(&shk),
                    layout: Layout::ControlPort,
                },
                peer: addr,
                event_secret: None,
                ntp: &mut ntp,
                ptp_hint: false,
                rtp_info: "seq=1;rtptime=2".into(),
                volume_db: volume,
            },
        )
        .await
        .unwrap();
        assert_eq!(negotiated.reply.data_port, 7001);
        log.lock().unwrap().clone()
    }

    fn methods(log: &[Entry]) -> Vec<&str> {
        log.iter().map(|(m, _, _)| m.as_str()).collect()
    }

    fn has_header(headers: &[(String, String)], name: &str) -> bool {
        headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    #[tokio::test]
    async fn owntone_order_is_setup_record_setup_volume() {
        let log = run(Profile::OwnTone, Some(-20.0)).await;
        assert_eq!(methods(&log), ["SETUP", "RECORD", "SETUP", "SET_PARAMETER"]);
        let record = &log[1].1;
        assert!(!has_header(record, "Range") && !has_header(record, "RTP-Info") && !has_header(record, "Session"));
        assert!(has_header(record, "Client-Instance") && has_header(record, "X-Apple-Client-Name"));
        assert_eq!(String::from_utf8_lossy(&log[3].2), "volume: -20.000000\r\n");
        let session: Value = Value::from_reader(std::io::Cursor::new(&log[0].2)).unwrap();
        let session = session.as_dictionary().unwrap();
        assert_eq!(session["timingProtocol"].as_string(), Some("NTP"));
        assert!(!session.contains_key("name"));
    }

    #[tokio::test]
    async fn owntone_without_volume_skips_set_parameter() {
        let log = run(Profile::OwnTone, None).await;
        assert_eq!(methods(&log), ["SETUP", "RECORD", "SETUP"]);
    }

    #[tokio::test]
    async fn doubletake_order_is_setup_setup_record() {
        let log = run(Profile::Doubletake, None).await;
        assert_eq!(methods(&log), ["SETUP", "SETUP", "RECORD"]);
        let record = &log[2].1;
        assert!(has_header(record, "Range") && has_header(record, "RTP-Info") && has_header(record, "Session"));
    }

    #[test]
    fn audio_key_follows_profile_and_secret() {
        let secret: Vec<u8> = (0..64).collect();
        let key = audio_key_for(Profile::OwnTone, Some(&secret));
        assert_eq!(key[..], secret[..32]);
        assert_ne!(audio_key_for(Profile::Doubletake, Some(&secret))[..], secret[..32]);
        assert_ne!(audio_key_for(Profile::OwnTone, None), audio_key_for(Profile::OwnTone, None));
        assert_eq!(session_url("192.168.1.5".parse().unwrap(), 42), "rtsp://192.168.1.5/42");
    }

    #[test]
    fn profile_selection() {
        assert_eq!(Profile::parse("doubletake"), Profile::Doubletake);
        assert_eq!(Profile::parse("DoubleTake"), Profile::Doubletake);
        assert_eq!(Profile::parse("owntone"), Profile::OwnTone);
        assert_eq!(Profile::parse(""), Profile::OwnTone);
    }
}
