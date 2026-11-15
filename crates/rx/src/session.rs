use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustplay_core::Error;
use rustplay_core::crypto::fairplay::FairPlayServer;
use rustplay_core::crypto::hap::{PairServer, Step};
use rustplay_core::crypto::pin::PinSetupServer;
use rustplay_core::crypto::stream::{self, AudioCipher};
use rustplay_core::ntp::NtpClock;
use rustplay_core::rtsp::{Request, Response};
use bytes::Bytes;
use plist::{Dictionary, Value};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::audio::AudioStream;
use crate::devices::DeviceKey;
use crate::mirror::{MirrorStream, MirrorWindow};
use crate::handlers::{self, BINARY_PLIST, Context, to_binary, uint};
use crate::metadata::{self, NowPlaying};
use crate::proto::{self, SetupRequest, StreamRequest, TextParameter};
use crate::access::{self, Decision};
use crate::{RxEvent, txt};

const OCTET_STREAM: &str = "application/octet-stream";
const DEFAULT_AUDIO_LATENCY_SAMPLES: u64 = 11_025;
// iOS posts /feedback every couple of seconds while streaming. A silent session counts as lost
// after this long: longer than iOS's own ~60 s sleep cutoff, so we never drop a session the sender would keep;
// it only catches connections that died without closing.
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(90);
pub const WATCHDOG_TICK: Duration = Duration::from_secs(1);

pub struct Session<'a> {
    ctx: &'a Context,
    peer: SocketAddr,
    cancel: CancellationToken,
    // Cancelling it closes the RTSP connection, which makes the sender stop streaming.
    connection: CancellationToken,
    pair: PairServer<'a>,
    fairplay: FairPlayServer,
    ecdh_secret: Option<[u8; 32]>,
    audio_keys: Option<([u8; 16], [u8; 16])>,
    #[allow(dead_code)]
    raw_aes_key: Option<[u8; 16]>,
    old_protocol: bool,
    ntp: Option<NtpClock>,
    audio: Option<AudioStream>,
    mirror: Option<MirrorStream>,
    client_name: Option<String>,
    device_id: Option<String>,
    // Registration in the receiver-wide list of streaming sessions.
    member: Option<u64>,
    last_feedback: Option<Instant>,
    volume_db: f32,
    // Muted by the user from the mirror window; shared with the audio renderer and the window.
    user_mute: Arc<std::sync::atomic::AtomicBool>,
    now_playing: NowPlaying,
    access: AccessGate,
    // Set when the connection must turn into an AirPlay video reverse channel (session ID).
    reverse: Option<String>,
    // The pending reverse connection belongs to the photo hub.
    reverse_photo: bool,
}

// Per-connection state of the PIN pairing and the access check.
#[derive(Default)]
struct AccessGate {
    pin_code: Option<String>,
    pin_setup: Option<PinSetupServer>,
    // Ed25519 key the client proved with the PIN on this connection.
    pin_key: Option<[u8; 32]>,
    // Pair-verify passed with a key we trust (only checked when a PIN is required).
    verified: bool,
    admitted: bool,
}

fn status(request: &Request, code: u16) -> Response {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        451 => "Parameter Not Understood",
        455 => "Method Not Valid in This State",
        470 => "Connection Authorization Required",
        500 => "Internal Server Error",
        _ => "Not Implemented",
    };
    Response::new(request.protocol, code, reason)
}

fn error_status(request: &Request, error: &Error, fallback: u16) -> Response {
    match error {
        Error::NotImplemented(_) => status(request, 501),
        _ => status(request, fallback),
    }
}

fn plist_response(request: &Request, root: Dictionary) -> Response {
    match to_binary(root) {
        Ok(body) => Response::ok(request.protocol).with_body(BINARY_PLIST, body),
        Err(e) => {
            tracing::warn!("no se pudo construir la respuesta plist: {e}");
            status(request, 500)
        }
    }
}

fn octet_response(request: &Request, body: Vec<u8>) -> Response {
    Response::ok(request.protocol).with_body(OCTET_STREAM, Bytes::from(body))
}

async fn start_event_channel(peer_ip: IpAddr, cancel: CancellationToken) -> std::io::Result<u16> {
    let listener = TcpListener::bind((rustplay_core::net::listen_addr(peer_ip), 0)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((mut stream, from)) = accepted else {
            return;
        };
        tracing::debug!(%from, "canal de eventos conectado");
        let mut sink = [0u8; 1024];
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                read = stream.read(&mut sink) => match read {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                },
            }
        }
    });
    Ok(port)
}

impl<'a> Session<'a> {
    pub fn new(ctx: &'a Context, peer: SocketAddr, cancel: &CancellationToken) -> Self {
        Self {
            ctx,
            peer,
            cancel: cancel.child_token(),
            connection: cancel.clone(),
            pair: PairServer::new(&ctx.identity),
            fairplay: FairPlayServer::default(),
            ecdh_secret: None,
            audio_keys: None,
            raw_aes_key: None,
            old_protocol: false,
            ntp: None,
            audio: None,
            mirror: None,
            client_name: None,
            device_id: None,
            member: None,
            last_feedback: None,
            volume_db: -30.0,
            user_mute: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            now_playing: NowPlaying::default(),
            access: AccessGate::default(),
            reverse: None,
            reverse_photo: false,
        }
    }

    fn emit(&self, event: RxEvent) {
        let _ = self.ctx.events.try_send(event);
    }

    pub fn finish(&mut self) {
        self.ctx.remotes.withdraw(self.peer);
        self.stop_audio();
        // A connection that drops (rather than one we closed) may come back: its window waits.
        let lost = !self.connection.is_cancelled();
        if let Some(mirror) = self.mirror.take() {
            mirror.end(lost);
        }
        self.ntp = None;
        self.leave();
        self.cancel.cancel();
    }

    // True when a mirroring sender stopped its heartbeat (iOS asleep, network gone).
    pub fn heartbeat_lost(&self) -> bool {
        let lost = self.mirror.is_some() && self.last_feedback.is_some_and(|t| t.elapsed() >= FEEDBACK_TIMEOUT);
        if lost {
            tracing::warn!(peer = %self.peer, "el emisor lleva {} s sin dar señales: se da la conexión por perdida", FEEDBACK_TIMEOUT.as_secs());
        }
        lost
    }

    fn key(&self) -> DeviceKey {
        DeviceKey::new(self.device_id.as_deref(), self.peer.ip())
    }

    // Registers the session when it starts its first stream. In single-device mode this
    // disconnects every other device and closes their waiting windows.
    fn join(&mut self) -> u64 {
        if let Some(id) = self.member {
            return id;
        }
        let key = self.key();
        let exclusive = !self.ctx.live.get().multiple_devices;
        let (id, replaced) = self.ctx.sessions.join(key.clone(), self.connection.clone(), exclusive);
        for other in &replaced {
            tracing::info!("{key} empieza a transmitir y sustituye a {other} (un dispositivo a la vez)");
        }
        if exclusive {
            let closed = self.ctx.windows.close_parked_except(&key);
            if closed > 0 {
                tracing::info!("{key} empieza a transmitir: se cierran {closed} ventana(s) en espera de otros dispositivos");
            }
        }
        self.member = Some(id);
        id
    }

    fn leave(&mut self) {
        if let Some(id) = self.member.take() {
            self.ctx.sessions.leave(id);
        }
    }

    fn stop_audio(&mut self) {
        if self.audio.take().is_some() {
            if let Some(id) = self.member {
                self.ctx.sessions.audio_stopped(id);
            }
            self.emit(RxEvent::StreamStopped { peer: self.peer });
        }
    }

    // Ends the mirror stream after a TEARDOWN. iOS also tears down when it goes to sleep with
    // the screen off; that window waits for the device like after a dropped connection.
    fn teardown_mirror(&mut self) {
        if let Some(mirror) = self.mirror.take() {
            let lost = mirror.suspended();
            mirror.end(lost);
        }
    }

    pub fn take_reverse(&mut self) -> Option<String> {
        self.reverse.take()
    }

    pub fn take_photo_reverse(&mut self) -> bool {
        std::mem::take(&mut self.reverse_photo)
    }

    pub fn photo(&self) -> Arc<crate::photo::Hub> {
        Arc::clone(&self.ctx.photo)
    }

    pub fn video(&self) -> Arc<crate::video::Hub> {
        Arc::clone(&self.ctx.video)
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub async fn handle(&mut self, request: &Request) -> Response {
        self.note_remote(request);
        let method = request.method.as_str();
        let uri = request.uri.as_str();
        if method == "POST" && uri == "/feedback" {
            self.last_feedback = Some(Instant::now());
        }
        let mut photo = self.ctx.photo.handle(request, self.peer, self.ctx.video.enabled()).await;
        if let Some((_, reverse)) = &mut photo {
            self.reverse = reverse.take();
            self.reverse_photo = self.reverse.is_some();
        }
        let mut video = if photo.is_none() { self.ctx.video.handle(request, self.peer) } else { None };
        if let Some((_, reverse)) = &mut video {
            self.reverse = reverse.take();
        }
        let mut response = match (method, uri) {
            _ if photo.is_some() => photo.take().map_or_else(|| status(request, 500), |(response, _)| response),
            _ if video.is_some() => video.take().map_or_else(|| status(request, 500), |(response, _)| response),
            ("GET", u) if u.contains("/info") => {
                handlers::info(self.ctx, request).unwrap_or_else(|e| {
                    tracing::warn!("no se pudo construir /info: {e}");
                    status(request, 500)
                })
            }
            ("POST", "/pair-setup") => self.pair_setup(request),
            ("POST", "/pair-pin-start") => self.pair_pin_start(request),
            ("POST", "/pair-setup-pin") => self.pair_setup_pin(request),
            ("POST", "/pair-verify") => self.pair_verify(request),
            ("POST", "/fp-setup") => self.fp_setup(request),
            ("POST", "/audioMode") => {
                tracing::debug!("audioMode recibido");
                Response::ok(request.protocol)
            }
            ("POST", other) => {
                tracing::debug!(uri = other, "petición POST sin acción específica");
                Response::ok(request.protocol)
            }
            ("OPTIONS", _) => {
                let mut r = Response::ok(request.protocol);
                r.headers.set(
                    "Public",
                    "SETUP, RECORD, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
                );
                r
            }
            ("SETUP", _) => self.setup(request).await,
            ("RECORD", _) => self.record(request),
            ("GET_PARAMETER", _) => self.get_parameter(request),
            ("SET_PARAMETER", _) => self.set_parameter(request),
            ("FLUSH", _) => self.flush(request),
            ("TEARDOWN", _) => self.teardown(request),
            _ => {
                tracing::info!(
                    method,
                    uri,
                    headers = ?request.headers.iter().collect::<Vec<_>>(),
                    body_len = request.body.len(),
                    "petición sin manejador"
                );
                status(request, 501)
            }
        };
        if let Some(cseq) = request.cseq() {
            response.headers.set("CSeq", cseq);
            if method != "RECORD" {
                response
                    .headers
                    .set("Audio-Jack-Status", "connected; type=digital");
            }
        }
        response
            .headers
            .set("Server", format!("AirTunes/{}", txt::SOURCE_VERSION));
        response
    }

    // iOS adds DACP-ID and Active-Remote to the requests of audio streaming sessions.
    fn note_remote(&mut self, request: &Request) {
        if self.now_playing.remote {
            return;
        }
        let (Some(id), Some(token)) = (request.headers.get("DACP-ID"), request.headers.get("Active-Remote")) else {
            return;
        };
        self.ctx.remotes.offer(self.peer, id.trim(), token.trim());
        self.now_playing.remote = true;
        if !self.now_playing.is_empty() {
            self.emit_now_playing();
        }
    }

    fn emit_now_playing(&self) {
        self.emit(RxEvent::NowPlaying(Box::new(self.now_playing.clone())));
    }

    // Forgets the track details of an audio stream that ended; DACP stays available.
    fn clear_now_playing(&mut self) {
        let remote = self.now_playing.remote;
        self.now_playing = NowPlaying { remote, ..NowPlaying::default() };
    }

    fn pair_setup(&mut self, request: &Request) -> Response {
        match self.pair.pair_setup(&request.body) {
            Ok(Step::Reply(body)) => octet_response(request, body),
            Ok(Step::Done(_)) => octet_response(request, Vec::new()),
            Err(e) => {
                tracing::warn!("fallo en pair-setup: {e}");
                error_status(request, &e, 470)
            }
        }
    }

    fn pair_verify(&mut self, request: &Request) -> Response {
        if self.ctx.access.pin.required
            && let Some(key) = verify_client_key(&request.body)
            && self.access.pin_key != Some(key)
            && !self.ctx.access.is_paired(&key)
        {
            tracing::warn!("{} no está emparejado con un código; se le pide el código", self.peer.ip());
            return status(request, 470);
        }
        match self.pair.pair_verify(&request.body) {
            Ok(Step::Reply(body)) => octet_response(request, body),
            Ok(Step::Done(keys)) => {
                self.ecdh_secret = Some(keys.shared_secret);
                self.access.verified = true;
                tracing::debug!("pair-verify completado");
                octet_response(request, Vec::new())
            }
            Err(e) => {
                tracing::warn!("fallo en pair-verify: {e}");
                let mut r = error_status(request, &e, 470);
                r.headers.set("Connection", "close");
                r
            }
        }
    }

    fn pair_pin_start(&mut self, request: &Request) -> Response {
        if !self.ctx.access.pin.required {
            tracing::debug!("pair-pin-start recibido sin código configurado");
            return Response::ok(request.protocol);
        }
        let (code, _) = self.ctx.access.issue_code(self.peer.ip());
        tracing::info!("{} pide emparejarse: se muestra el código en pantalla", self.peer.ip());
        self.access.pin_code = Some(code.clone());
        self.access.pin_setup = None;
        // Shown again even when reused, in case the earlier prompt was closed.
        self.emit(RxEvent::PinShown { peer: self.peer, pin: code });
        Response::ok(request.protocol)
    }

    fn pair_setup_pin(&mut self, request: &Request) -> Response {
        match self.pin_step(&request.body) {
            Ok(reply) => plist_response(request, reply),
            Err(e) => {
                tracing::warn!("emparejamiento con código fallido con {}: {e}", self.peer.ip());
                self.access.pin_setup = None;
                self.access.pin_code = None;
                // The code stays on screen for another try (iOS's own first try always fails).
                if self.ctx.access.code_failed(self.peer.ip()) {
                    self.emit(RxEvent::PinFinished { peer: self.peer, paired: false });
                }
                Response::new(request.protocol, 470, "Client Authentication Failure")
            }
        }
    }

    fn pin_step(&mut self, body: &[u8]) -> rustplay_core::Result<Dictionary> {
        if !self.ctx.access.pin.required {
            return Err(Error::Pairing("este receptor no pide código".into()));
        }
        let mut reply = Dictionary::new();
        match proto::parse_pin_setup(body)? {
            proto::PinSetup::Start { user } => {
                let code = match &self.access.pin_code {
                    Some(code) => code.clone(),
                    None => {
                        // Usually the code shown on the connection iOS already closed.
                        let (code, new) = self.ctx.access.issue_code(self.peer.ip());
                        self.access.pin_code = Some(code.clone());
                        if new {
                            self.emit(RxEvent::PinShown { peer: self.peer, pin: code.clone() });
                        }
                        code
                    }
                };
                tracing::info!("emparejamiento con código: {} se identifica como {user}", self.peer.ip());
                let server = PinSetupServer::start(&user, &code)?;
                reply.insert("pk".into(), Value::Data(server.public_key().to_vec()));
                reply.insert("salt".into(), Value::Data(server.salt().to_vec()));
                self.access.pin_setup = Some(server);
            }
            proto::PinSetup::Proof { public_key, proof } => {
                let server = self
                    .access
                    .pin_setup
                    .as_mut()
                    .ok_or_else(|| Error::Pairing("pair-setup-pin fuera de secuencia".into()))?;
                let proof = server.verify(&public_key, &proof)?;
                reply.insert("proof".into(), Value::Data(proof));
            }
            proto::PinSetup::Confirm { epk, auth_tag } => {
                let mut server = self
                    .access
                    .pin_setup
                    .take()
                    .ok_or_else(|| Error::Pairing("pair-setup-pin fuera de secuencia".into()))?;
                let confirmed = server.confirm(&self.ctx.public_key, &epk, &auth_tag)?;
                self.ctx.access.remember_pairing(server.user(), confirmed.client_key);
                self.access.pin_key = Some(confirmed.client_key);
                self.access.pin_code = None;
                self.ctx.access.clear_code(self.peer.ip());
                tracing::info!("{} ({}) emparejado con código", self.peer.ip(), server.user());
                self.emit(RxEvent::PinFinished { peer: self.peer, paired: true });
                reply.insert("epk".into(), Value::Data(confirmed.epk.to_vec()));
                reply.insert("authTag".into(), Value::Data(confirmed.auth_tag.to_vec()));
            }
        }
        Ok(reply)
    }

    // With a PIN required, nothing past pairing is served before a trusted pair-verify.
    fn needs_pairing(&self) -> bool {
        self.ctx.access.pin.required && !self.access.verified
    }

    async fn admit(&mut self, setup: &SetupRequest) -> Result<(), u16> {
        let device_id = access::normalize(setup.device_id.as_deref().unwrap_or_default());
        let name = setup.name.clone();
        let ip = self.peer.ip();
        if !device_id.is_empty() {
            self.emit(RxEvent::DeviceSeen {
                peer: self.peer,
                device_id: device_id.clone(),
                name: name.clone(),
                model: setup.model.clone(),
            });
        }
        let key = if device_id.is_empty() { ip.to_string() } else { device_id.clone() };
        let config = self.ctx.live.get().access;
        let allowed = match access::decide(&config, &device_id, self.ctx.access.remembered(&key)) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask => self.ask(&key, &device_id, name.clone()).await,
        };
        if allowed {
            self.access.admitted = true;
            return Ok(());
        }
        tracing::warn!(
            "conexión rechazada: {ip} ({device_id}, {}) no tiene permiso",
            name.as_deref().unwrap_or("sin nombre")
        );
        self.emit(RxEvent::AccessDenied { peer: self.peer, device_id, name });
        Err(401)
    }

    async fn ask(&mut self, key: &str, device_id: &str, name: Option<String>) -> bool {
        let ticket = self.ctx.access.enqueue(key);
        let id = ticket.id;
        if ticket.first {
            tracing::info!("se pregunta si {} ({device_id}) puede conectarse", self.peer.ip());
            self.emit(RxEvent::AccessRequest {
                id,
                peer: self.peer,
                device_id: device_id.to_owned(),
                name,
            });
        }
        let answer = tokio::select! {
            answer = tokio::time::timeout(access::ASK_TIMEOUT, ticket.answer) => match answer {
                Ok(Ok(allow)) => Some(allow),
                Ok(Err(_)) => Some(false),
                Err(_) => None,
            },
            () = self.cancel.cancelled() => None,
        };
        answer.unwrap_or_else(|| {
            if self.ctx.access.abandon(id) {
                tracing::info!("nadie respondió a tiempo a la conexión de {}", self.peer.ip());
                self.emit(RxEvent::AccessResolved { id });
            }
            false
        })
    }

    fn fp_setup(&mut self, request: &Request) -> Response {
        if self.needs_pairing() {
            tracing::warn!("fp-setup de {} sin emparejamiento con código", self.peer.ip());
            return status(request, 470);
        }
        let mut out = Vec::with_capacity(142);
        match self.fairplay.setup(&request.body, &mut out) {
            Ok(()) => octet_response(request, out),
            Err(e) => {
                tracing::warn!("fallo en fp-setup: {e}");
                status(request, 501)
            }
        }
    }

    async fn setup(&mut self, request: &Request) -> Response {
        let setup = match proto::parse_setup(&request.body) {
            Ok(setup) => setup,
            Err(e) => {
                tracing::warn!("SETUP no válido: {e}");
                return status(request, 400);
            }
        };
        if self.needs_pairing() {
            tracing::warn!("SETUP de {} sin emparejamiento con código", self.peer.ip());
            return status(request, 470);
        }
        if setup.has_session_keys()
            && !self.access.admitted
            && let Err(code) = self.admit(&setup).await
        {
            return status(request, code);
        }
        let mut reply = Dictionary::new();
        if setup.has_session_keys() {
            match self.setup_session(request, &setup).await {
                Ok((timing_port, event_port)) => {
                    reply.insert("timingPort".into(), uint(u64::from(timing_port)));
                    reply.insert("eventPort".into(), uint(u64::from(event_port)));
                }
                Err(code) => return status(request, code),
            }
        }
        if let Some(streams) = &setup.streams {
            let mut out = Vec::with_capacity(streams.len());
            for stream in streams {
                match self.setup_stream(stream).await {
                    Ok(entry) => out.push(Value::Dictionary(entry)),
                    Err(code) => return status(request, code),
                }
            }
            reply.insert("streams".into(), Value::Array(out));
        }
        plist_response(request, reply)
    }

    async fn setup_session(&mut self, request: &Request, setup: &SetupRequest) -> Result<(u16, u16), u16> {
        let ekey: Option<[u8; 72]> = setup.ekey.as_deref().and_then(|e| e.try_into().ok());
        let eiv: Option<[u8; 16]> = setup.eiv.as_deref().and_then(|e| e.try_into().ok());
        let (Some(ekey), Some(eiv)) = (ekey, eiv) else {
            tracing::warn!("SETUP: ekey o eiv con tamaño no válido (se esperan 72 y 16 bytes)");
            return Err(400);
        };
        if setup.remote_control_only {
            tracing::warn!(
                "el cliente pide el modo de control remoto de AirPlay 2, que no está admitido"
            );
        }
        let aes_key = match self.fairplay.decrypt_key(&ekey) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("no se pudo descifrar la clave de audio (FairPlay): {e}");
                return Err(if matches!(e, Error::NotImplemented(_)) {
                    501
                } else {
                    500
                });
            }
        };
        self.raw_aes_key = Some(aes_key);
        if let Some(agent) = request.headers.get("User-Agent") {
            tracing::info!("cliente identificado como {agent}");
            self.old_protocol = agent.contains("AirMyPC");
        }
        let audio_key = match &self.ecdh_secret {
            Some(secret) if !self.old_protocol => stream::audio_key(&aes_key, secret),
            _ => aes_key,
        };
        self.audio = None;
        self.ntp = None;
        self.audio_keys = Some((audio_key, eiv));

        let mut timing_port = 0;
        if setup.uses_ntp() {
            let mut target = self.peer;
            target.set_port(setup.timing_port);
            match NtpClock::bind(target).await {
                Ok(clock) => {
                    timing_port = clock.local_port();
                    self.ntp = Some(clock);
                }
                Err(e) => tracing::warn!("no se pudo iniciar la sincronización de reloj: {e}"),
            }
        } else {
            tracing::debug!("el cliente no usa NTP para el reloj; se omite la sincronización");
        }
        let event_port = match start_event_channel(self.peer.ip(), self.cancel.child_token()).await
        {
            Ok(port) => port,
            Err(e) => {
                tracing::warn!("no se pudo abrir el canal de eventos: {e}");
                0
            }
        };
        if let Some(id) = &setup.device_id {
            self.device_id = Some(id.clone());
        }
        if let Some(name) = &setup.name {
            self.client_name = Some(name.clone());
            self.emit(RxEvent::ClientName {
                peer: self.peer,
                name: name.clone(),
            });
        }
        Ok((timing_port, event_port))
    }

    async fn setup_stream(&mut self, stream: &StreamRequest) -> Result<Dictionary, u16> {
        match stream {
            StreamRequest::Audio(request) => {
                if !self.ctx.live.get().audio {
                    tracing::warn!("el audio está desactivado en este receptor");
                    return Err(501);
                }
                let Some((key, iv)) = self.audio_keys else {
                    tracing::warn!("SETUP de audio sin intercambio de claves previo");
                    return Err(455);
                };
                self.stop_audio();
                let member = self.join();
                let started = AudioStream::start(
                    request,
                    self.peer,
                    AudioCipher::new(key, iv),
                    self.volume_db,
                    Arc::clone(&self.user_mute),
                    Arc::clone(&self.ctx.live),
                    &self.cancel,
                )
                .await;
                let audio = match started {
                    Ok(audio) => audio,
                    Err(e) => {
                        tracing::warn!("no se pudo iniciar el flujo de audio: {e}");
                        return Err(500);
                    }
                };
                let mut entry = Dictionary::new();
                entry.insert("dataPort".into(), uint(u64::from(audio.data_port)));
                entry.insert("controlPort".into(), uint(u64::from(audio.control_port)));
                entry.insert("type".into(), uint(96));
                tracing::debug!(
                    codec = ?audio.codec,
                    latency_ms = audio.latency_ms,
                    data = audio.data_port,
                    control = audio.control_port,
                    "flujo de audio preparado"
                );
                let codec = audio.codec;
                self.ctx.sessions.audio_started(member, audio.mute_flag());
                self.audio = Some(audio);
                self.emit(RxEvent::AudioStarted { peer: self.peer, codec });
                Ok(entry)
            }
            StreamRequest::Mirror { connection_id } => {
                if !self.ctx.live.get().video {
                    tracing::warn!("la duplicación de pantalla está desactivada en este receptor");
                    return Err(501);
                }
                let Some((audio_key, _)) = self.audio_keys else {
                    tracing::warn!("SETUP de vídeo sin intercambio de claves previo");
                    return Err(455);
                };
                // The window of a previous stream is handed to the new one.
                if let Some(old) = self.mirror.take() {
                    old.end(true);
                }
                self.join();
                let key = stream::mirror_key_from_audio_key(&audio_key, *connection_id);
                let title = format!(
                    "RustPlay – {}",
                    self.client_name.as_deref().unwrap_or(&self.ctx.name)
                );
                let device = self
                    .client_name
                    .clone()
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| self.ctx.live.get().texts.unknown_device);
                let window = MirrorWindow {
                    live: Arc::clone(&self.ctx.live),
                    prewarm: Arc::clone(&self.ctx.prewarm),
                    windows: Arc::clone(&self.ctx.windows),
                    key: self.key(),
                    device,
                    mute: Arc::clone(&self.user_mute),
                };
                let started = MirrorStream::start(
                    &key,
                    self.peer,
                    title,
                    window,
                    self.ctx.events.clone(),
                    &self.cancel,
                    self.connection.clone(),
                )
                .await;
                let mirror = match started {
                    Ok(mirror) => mirror,
                    Err(e) => {
                        tracing::warn!("no se pudo iniciar la duplicación de pantalla: {e}");
                        return Err(500);
                    }
                };
                let mut entry = Dictionary::new();
                entry.insert("dataPort".into(), uint(u64::from(mirror.data_port)));
                entry.insert("type".into(), uint(110));
                tracing::debug!(data = mirror.data_port, "flujo de vídeo preparado");
                self.mirror = Some(mirror);
                Ok(entry)
            }
            StreamRequest::Other(kind) => {
                tracing::warn!("tipo de flujo desconocido en SETUP: {kind}");
                Err(501)
            }
        }
    }

    fn record(&mut self, request: &Request) -> Response {
        let samples = self
            .audio
            .as_ref()
            .map_or(DEFAULT_AUDIO_LATENCY_SAMPLES, |a| {
                u64::from(a.latency_ms) * 44_100 / 1000
            });
        if !self.now_playing.playing && self.audio.is_some() {
            self.now_playing.playing = true;
            if !self.now_playing.is_empty() {
                self.emit_now_playing();
            }
        }
        let mut response = Response::ok(request.protocol);
        response.headers.set("Audio-Latency", samples.to_string());
        response
            .headers
            .set("Audio-Jack-Status", "connected; type=analog");
        response
    }

    fn get_parameter(&mut self, request: &Request) -> Response {
        let Some(content_type) = request.headers.get("Content-Type") else {
            return status(request, 451);
        };
        let mut response = Response::ok(request.protocol);
        if content_type.starts_with("text/parameters") {
            let body = String::from_utf8_lossy(&request.body);
            if proto::requests_volume(&body) {
                response = response.with_body(
                    "text/parameters",
                    Bytes::from(format!("volume: {:.6}\r\n", self.volume_db)),
                );
            } else {
                tracing::warn!("GET_PARAMETER con parámetro desconocido: {}", body.trim());
            }
        }
        response
    }

    fn set_parameter(&mut self, request: &Request) -> Response {
        let Some(content_type) = request.headers.get("Content-Type") else {
            return status(request, 451);
        };
        if content_type.starts_with("text/parameters") {
            let body = String::from_utf8_lossy(&request.body);
            let (known, unknown) = proto::parse_text_parameters(&body);
            for parameter in known {
                match parameter {
                    TextParameter::Volume(db) => self.set_volume(db),
                    TextParameter::Progress {
                        start,
                        current,
                        end,
                    } => {
                        tracing::debug!(start, current, end, "progreso de reproducción");
                        let (position, duration) =
                            metadata::progress_ms(start, current, end, rustplay_core::media::audio::SAMPLE_RATE);
                        self.now_playing.position_ms = Some(position);
                        self.now_playing.duration_ms = (duration > 0).then_some(duration);
                        self.now_playing.playing = true;
                        self.emit_now_playing();
                    }
                }
            }
            for line in unknown {
                tracing::debug!("parámetro SET_PARAMETER desconocido: {line}");
            }
        } else if content_type.starts_with("image/") {
            tracing::debug!(bytes = request.body.len(), content_type, "carátula recibida");
            let image = content_type.starts_with("image/jpeg") || content_type.starts_with("image/png");
            self.now_playing.artwork = (image && !request.body.is_empty()).then(|| request.body.to_vec().into());
            self.emit_now_playing();
        } else if content_type.starts_with("application/x-dmap-tagged") {
            let info = metadata::parse_dmap(&request.body);
            tracing::info!(
                title = info.title.as_deref().unwrap_or(""),
                artist = info.artist.as_deref().unwrap_or(""),
                album = info.album.as_deref().unwrap_or(""),
                "{} reproduce una pista",
                self.peer
            );
            self.now_playing.title = info.title;
            self.now_playing.artist = info.artist;
            self.now_playing.album = info.album;
            self.emit_now_playing();
        } else {
            tracing::debug!(content_type, "SET_PARAMETER ignorado");
        }
        Response::ok(request.protocol)
    }

    fn set_volume(&mut self, db: f32) {
        let db = if db.is_finite() {
            db.clamp(-144.0, 0.0)
        } else {
            -144.0
        };
        self.volume_db = db;
        if let Some(audio) = &self.audio {
            audio.set_volume_db(db);
        }
        self.ctx.video.set_volume(crate::proto::db_to_linear(db));
        self.emit(RxEvent::Volume { db });
    }

    fn flush(&mut self, request: &Request) -> Response {
        let next = request
            .headers
            .get("RTP-Info")
            .inspect(|info| tracing::debug!("FLUSH con RTP-Info: {info}"))
            .and_then(proto::rtp_info_sequence);
        match &self.audio {
            Some(audio) => audio.flush(next),
            None => tracing::debug!("FLUSH sin flujo de audio activo"),
        }
        if self.now_playing.playing {
            self.now_playing.playing = false;
            if !self.now_playing.is_empty() {
                self.emit_now_playing();
            }
        }
        Response::ok(request.protocol)
    }

    fn teardown(&mut self, request: &Request) -> Response {
        let kinds = proto::parse_teardown(&request.body);
        tracing::debug!(?kinds, "TEARDOWN");
        let mut response = Response::ok(request.protocol);
        let audio = kinds.contains(&96);
        let mirror = kinds.contains(&110);
        if audio {
            self.clear_now_playing();
            self.stop_audio();
        }
        if mirror {
            self.teardown_mirror();
        }
        // Only a TEARDOWN without streams ends the session. iOS tears down just the audio
        // stream (type 96) after a few seconds of silence while mirroring goes on.
        if !audio && !mirror {
            response.headers.set("Connection", "close");
            self.stop_audio();
            self.teardown_mirror();
            self.ntp = None;
            self.audio_keys = None;
        }
        response
    }
}

// The client Ed25519 key of a legacy pair-verify step 1 (`01 00 00 00` + X25519 + Ed25519).
fn verify_client_key(body: &[u8]) -> Option<[u8; 32]> {
    if body.len() == 68 && body[0] == 1 {
        body[36..68].try_into().ok()
    } else {
        None
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.leave();
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use rustplay_core::identity::Identity;
    use rustplay_core::rtsp::{Headers, Protocol};
    use ed25519_dalek::SigningKey;
    use tokio::sync::mpsc;

    use super::*;

    fn context() -> (Context, mpsc::Receiver<RxEvent>) {
        context_with(crate::access::AccessState::new(Default::default(), None))
    }

    fn context_with(access: crate::access::AccessState) -> (Context, mpsc::Receiver<RxEvent>) {
        let device_id = [1, 2, 3, 4, 5, 6];
        let records = txt::build(&device_id, &[7; 32], "PID");
        let (events, rx) = mpsc::channel(16);
        let events_for_hub = events.clone();
        let ctx = Context {
            name: "PC".into(),
            identity: Identity {
                device_id,
                signing_key: SigningKey::from_bytes(&[9; 32]),
                pairing_id: "PID".into(),
            },
            device_id,
            public_key: [7; 32],
            pairing_id: "PID".into(),
            txt_raop: records.raop,
            txt_airplay: records.airplay,
            events,
            live: Arc::new(crate::Live::new(crate::LiveConfig::from(&crate::ReceiverConfig::default()))),
            prewarm: crate::mirror::Prewarm::new(),
            remotes: Arc::new(crate::dacp::Remotes::default()),
            sessions: Arc::default(),
            windows: Arc::default(),
            access: Arc::new(access),
            video: crate::video::Hub::disabled(events_for_hub.clone()),
            photo: crate::photo::Hub::disabled(events_for_hub),
        };
        (ctx, rx)
    }

    fn request(method: &str, uri: &str, body: Vec<u8>, content_type: Option<&str>) -> Request {
        let mut headers = Headers::default();
        headers.set("CSeq", "4");
        if let Some(ct) = content_type {
            headers.set("Content-Type", ct);
        }
        Request {
            method: method.into(),
            uri: uri.into(),
            protocol: Protocol::Rtsp10,
            headers,
            body: Bytes::from(body),
        }
    }

    fn session<'a>(ctx: &'a Context) -> Session<'a> {
        Session::new(
            ctx,
            "127.0.0.1:50000".parse().unwrap(),
            &CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn info_is_a_valid_plist_with_expected_keys() {
        let (ctx, _rx) = context();
        let response = session(&ctx)
            .handle(&request("GET", "/info", Vec::new(), None))
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.headers.get("CSeq"), Some("4"));
        assert_eq!(response.headers.get("Server"), Some("AirTunes/220.68"));
        let value = Value::from_reader(std::io::Cursor::new(&response.body[..])).unwrap();
        let dict = value.as_dictionary().unwrap();
        assert_eq!(
            dict.get("deviceID").and_then(Value::as_string),
            Some("01:02:03:04:05:06")
        );
        assert_eq!(dict.get("name").and_then(Value::as_string), Some("PC"));
        let displays = dict.get("displays").and_then(Value::as_array).unwrap();
        let first = displays[0].as_dictionary().unwrap();
        assert_eq!(
            first.get("width").and_then(Value::as_unsigned_integer),
            Some(1920)
        );
    }

    #[tokio::test]
    async fn info_advertises_configured_display() {
        let (ctx, _rx) = context();
        ctx.live.update(|c| {
            c.display_width = 2560;
            c.display_height = 1440;
            c.max_fps = 30;
        });
        let response = session(&ctx)
            .handle(&request("GET", "/info", Vec::new(), None))
            .await;
        let value = Value::from_reader(std::io::Cursor::new(&response.body[..])).unwrap();
        let displays = value.as_dictionary().unwrap().get("displays").and_then(Value::as_array).unwrap();
        let d = displays[0].as_dictionary().unwrap();
        let uint = |k: &str| d.get(k).and_then(Value::as_unsigned_integer);
        assert_eq!(uint("width"), Some(2560));
        assert_eq!(uint("heightPixels"), Some(1440));
        assert_eq!(uint("maxFPS"), Some(30));
        assert_eq!(d.get("refreshRate").and_then(Value::as_real), Some(1.0 / 30.0));
    }

    #[test]
    fn receiver_config_defaults() {
        let c = crate::ReceiverConfig::default();
        assert_eq!((c.display_width, c.display_height, c.max_fps), (1920, 1080, 60));
        let w = c.window;
        assert!(!w.fullscreen && !w.always_on_top && w.lock_screen_backdrop && w.controls);
        assert!(!w.vsync && !w.high_quality_scaling && !w.stats && !w.open_app_button);
        assert_eq!(w.scaling, crate::Scaling::Fit);
    }

    #[test]
    fn live_options_count_changes_only_when_they_differ() {
        let (ctx, _rx) = context();
        let before = ctx.live.generation();
        ctx.live.update(|c| c.max_fps = 60);
        assert_eq!(ctx.live.generation(), before);
        ctx.live.update(|c| c.window.stats = true);
        assert_eq!(ctx.live.generation(), before + 1);
        assert!(ctx.live.get().window.stats);
    }

    #[tokio::test]
    async fn info_qualifier_returns_txt_data() {
        let (ctx, _rx) = context();
        let mut query = Dictionary::new();
        query.insert(
            "qualifier".into(),
            Value::Array(vec![Value::String("txtAirPlay".into())]),
        );
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(query)).unwrap();
        let response = session(&ctx)
            .handle(&request("GET", "/info", body, Some(BINARY_PLIST)))
            .await;
        let value = Value::from_reader(std::io::Cursor::new(&response.body[..])).unwrap();
        assert!(matches!(
            value.as_dictionary().unwrap().get("txtAirPlay"),
            Some(Value::Data(_))
        ));
    }

    #[tokio::test]
    async fn unknown_methods_get_501_and_unknown_posts_get_200() {
        let (ctx, _rx) = context();
        let mut s = session(&ctx);
        assert_eq!(
            s.handle(&request("FOO", "/x", Vec::new(), None))
                .await
                .status,
            501
        );
        for uri in ["/feedback", "/command", "/audioMode", "/nope"] {
            let r = s.handle(&request("POST", uri, Vec::new(), None)).await;
            assert_eq!(r.status, 200, "{uri}");
            assert_eq!(r.headers.get("CSeq"), Some("4"));
        }
    }

    fn now_playing(rx: &mut mpsc::Receiver<RxEvent>) -> Option<NowPlaying> {
        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            if let RxEvent::NowPlaying(now) = event {
                last = Some(*now);
            }
        }
        last
    }

    #[tokio::test]
    async fn set_parameter_metadata_builds_now_playing() {
        let (ctx, mut rx) = context();
        let mut s = session(&ctx);
        let mut dmap = Vec::new();
        for (tag, value) in [(b"minm", "Título"), (b"asar", "Artista"), (b"asal", "Álbum")] {
            dmap.extend_from_slice(tag);
            dmap.extend_from_slice(&(value.len() as u32).to_be_bytes());
            dmap.extend_from_slice(value.as_bytes());
        }
        let mut body = b"mlit".to_vec();
        body.extend_from_slice(&(dmap.len() as u32).to_be_bytes());
        body.extend(dmap);
        let mut r = request("SET_PARAMETER", "rtsp://x/1", body, Some("application/x-dmap-tagged"));
        r.headers.set("DACP-ID", "0123456789ABCDEF");
        r.headers.set("Active-Remote", "1234");
        assert_eq!(s.handle(&r).await.status, 200);
        let r = request("SET_PARAMETER", "rtsp://x/1", vec![0xFF, 0xD8, 0xFF], Some("image/jpeg"));
        assert_eq!(s.handle(&r).await.status, 200);
        let progress = format!("progress: {}/{}/{}\r\n", 1000, 1000 + 44_100 * 5, 1000 + 44_100 * 200);
        let r = request("SET_PARAMETER", "rtsp://x/1", progress.into_bytes(), Some("text/parameters"));
        assert_eq!(s.handle(&r).await.status, 200);
        let now = now_playing(&mut rx).expect("evento NowPlaying");
        assert_eq!(now.title.as_deref(), Some("Título"));
        assert_eq!(now.artist.as_deref(), Some("Artista"));
        assert_eq!(now.album.as_deref(), Some("Álbum"));
        assert_eq!(now.artwork.as_deref(), Some(&[0xFF, 0xD8, 0xFF][..]));
        assert_eq!((now.position_ms, now.duration_ms), (Some(5_000), Some(200_000)));
        assert!(now.playing && now.remote);

        s.handle(&request("FLUSH", "rtsp://x/1", Vec::new(), None)).await;
        assert!(!now_playing(&mut rx).unwrap().playing);
        let r = request("SET_PARAMETER", "rtsp://x/1", Vec::new(), Some("image/none"));
        s.handle(&r).await;
        assert_eq!(now_playing(&mut rx).unwrap().artwork, None);
        s.finish();
    }

    #[tokio::test]
    async fn options_lists_methods() {
        let (ctx, _rx) = context();
        let r = session(&ctx)
            .handle(&request("OPTIONS", "*", Vec::new(), None))
            .await;
        assert_eq!(r.status, 200);
        assert!(r.headers.get("Public").unwrap().contains("SET_PARAMETER"));
        assert_eq!(
            r.headers.get("Audio-Jack-Status"),
            Some("connected; type=digital")
        );
    }

    #[tokio::test]
    async fn record_returns_latency_headers() {
        let (ctx, _rx) = context();
        let r = session(&ctx)
            .handle(&request("RECORD", "rtsp://x/1", Vec::new(), None))
            .await;
        assert_eq!(r.headers.get("Audio-Latency"), Some("11025"));
        assert_eq!(
            r.headers.get("Audio-Jack-Status"),
            Some("connected; type=analog")
        );
    }

    #[tokio::test]
    async fn volume_round_trip_and_event() {
        let (ctx, mut rx) = context();
        let mut s = session(&ctx);
        let r = s
            .handle(&request(
                "SET_PARAMETER",
                "rtsp://x/1",
                b"volume: -12.500000\r\n".to_vec(),
                Some("text/parameters"),
            ))
            .await;
        assert_eq!(r.status, 200);
        assert!(matches!(rx.try_recv(), Ok(RxEvent::Volume { db }) if (db + 12.5).abs() < 1e-6));
        let r = s
            .handle(&request(
                "GET_PARAMETER",
                "rtsp://x/1",
                b"volume\r\n".to_vec(),
                Some("text/parameters"),
            ))
            .await;
        assert_eq!(&r.body[..], b"volume: -12.500000\r\n");
        let r = s
            .handle(&request("SET_PARAMETER", "rtsp://x/1", Vec::new(), None))
            .await;
        assert_eq!(r.status, 451);
    }

    #[tokio::test]
    async fn audio_setup_without_keys_is_rejected_gracefully() {
        let (ctx, _rx) = context();
        let mut stream = Dictionary::new();
        stream.insert("type".into(), uint(96));
        stream.insert("ct".into(), uint(2));
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        let body = to_binary(root).unwrap().to_vec();
        let mut s = session(&ctx);
        let r = s
            .handle(&request("SETUP", "rtsp://x/1", body, Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 455);
        let r = s
            .handle(&request(
                "SETUP",
                "rtsp://x/1",
                b"garbage".to_vec(),
                Some(BINARY_PLIST),
            ))
            .await;
        assert_eq!(r.status, 400);
    }

    fn mirror_setup_body() -> Vec<u8> {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), uint(110));
        stream.insert("streamConnectionID".into(), Value::Integer((-7i64).into()));
        let mut root = Dictionary::new();
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        to_binary(root).unwrap().to_vec()
    }

    #[tokio::test]
    async fn mirror_setup_without_keys_is_rejected_but_session_survives() {
        let (ctx, _rx) = context();
        let mut s = session(&ctx);
        let r = s
            .handle(&request("SETUP", "rtsp://x/1", mirror_setup_body(), Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 455);
        let r = s.handle(&request("OPTIONS", "*", Vec::new(), None)).await;
        assert_eq!(r.status, 200);
    }

    #[tokio::test]
    async fn mirror_setup_is_rejected_when_video_is_disabled() {
        let (ctx, _rx) = context();
        ctx.live.update(|c| c.video = false);
        let mut s = session(&ctx);
        s.audio_keys = Some(([1; 16], [2; 16]));
        let r = s
            .handle(&request("SETUP", "rtsp://x/1", mirror_setup_body(), Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 501);
    }

    #[tokio::test]
    async fn mirror_setup_replies_with_data_port_and_teardown_stops_it() {
        let (ctx, _rx) = context();
        let mut s = session(&ctx);
        s.audio_keys = Some(([1; 16], [2; 16]));
        let r = s
            .handle(&request("SETUP", "rtsp://x/1", mirror_setup_body(), Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 200);
        let value = Value::from_reader(std::io::Cursor::new(&r.body[..])).unwrap();
        let streams = value
            .as_dictionary()
            .and_then(|d| d.get("streams"))
            .and_then(Value::as_array)
            .unwrap();
        let entry = streams[0].as_dictionary().unwrap();
        assert_eq!(entry.get("type").and_then(Value::as_unsigned_integer), Some(110));
        assert!(entry.get("dataPort").and_then(Value::as_unsigned_integer).unwrap() > 0);
        assert!(s.mirror.is_some());
        let mut teardown = Dictionary::new();
        let mut kind = Dictionary::new();
        kind.insert("type".into(), uint(110));
        teardown.insert("streams".into(), Value::Array(vec![Value::Dictionary(kind)]));
        let body = to_binary(teardown).unwrap().to_vec();
        let r = s
            .handle(&request("TEARDOWN", "rtsp://x/1", body, Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 200);
        assert!(s.mirror.is_none());
        assert_eq!(r.headers.get("Connection"), None, "cerrar un flujo no cierra la conexión");
    }

    #[tokio::test]
    async fn audio_teardown_keeps_the_connection_open() {
        let (ctx, _rx) = context();
        let mut teardown = Dictionary::new();
        let mut kind = Dictionary::new();
        kind.insert("type".into(), uint(96));
        teardown.insert("streams".into(), Value::Array(vec![Value::Dictionary(kind)]));
        let body = to_binary(teardown).unwrap().to_vec();
        let r = session(&ctx)
            .handle(&request("TEARDOWN", "rtsp://x/1", body, Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("Connection"), None);
    }

    fn plist_body(entries: &[(&str, Value)]) -> Vec<u8> {
        let mut d = Dictionary::new();
        for (k, v) in entries {
            d.insert((*k).into(), v.clone());
        }
        to_binary(d).unwrap().to_vec()
    }

    fn reply_data(response: &Response, key: &str) -> Vec<u8> {
        let value = Value::from_reader(std::io::Cursor::new(&response.body[..])).unwrap();
        match value.as_dictionary().and_then(|d| d.get(key)) {
            Some(Value::Data(d)) => d.clone(),
            other => panic!("falta {key}: {other:?}"),
        }
    }

    fn first_setup_body(device_id: &str) -> Vec<u8> {
        plist_body(&[
            ("ekey", Value::Data(vec![1; 72])),
            ("eiv", Value::Data(vec![2; 16])),
            ("deviceID", Value::String(device_id.into())),
            ("name", Value::String("iPhone de Ana".into())),
            ("model", Value::String("iPhone15,2".into())),
        ])
    }

    fn pin_context(tag: &str) -> (Context, mpsc::Receiver<RxEvent>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("rustplay-pin-{tag}-{}", std::process::id()));
        let pin = crate::access::PinConfig { required: true, fixed: Some("1234".into()) };
        let state = crate::access::AccessState::new(pin, Some(dir.join("client-pairings.plist")));
        let (ctx, rx) = context_with(state);
        (ctx, rx, dir)
    }

    // Legacy pair-verify of `client` against the session; returns the last response status.
    async fn legacy_verify(s: &mut Session<'_>, ctx: &Context, client: &Identity) -> u16 {
        use rustplay_core::crypto::hap::PairClient;
        let server_key = ctx.identity.public_key().to_bytes();
        let mut pair = PairClient::new(client);
        let mut reply: Option<Vec<u8>> = None;
        loop {
            let body = match pair.pair_verify_legacy(&server_key, reply.as_deref()).unwrap() {
                Step::Reply(body) => body,
                Step::Done(_) => return 200,
            };
            let r = s.handle(&request("POST", "/pair-verify", body, Some(OCTET_STREAM))).await;
            if r.status != 200 {
                return r.status;
            }
            if r.body.is_empty() {
                return 200;
            }
            reply = Some(r.body.to_vec());
        }
    }

    fn client_identity(seed: u8) -> Identity {
        Identity {
            device_id: [seed; 6],
            signing_key: SigningKey::from_bytes(&[seed; 32]),
            pairing_id: "CLIENT".into(),
        }
    }

    #[tokio::test]
    async fn pin_pairing_is_remembered_for_later_connections() {
        use rustplay_core::crypto::pin::PinSetupClient;
        let (ctx, mut rx, dir) = pin_context("ok");
        let user = "AA:BB:CC:DD:EE:FF";
        let phone = client_identity(3);
        let mut s = session(&ctx);
        let r = s.handle(&request("POST", "/pair-pin-start", Vec::new(), None)).await;
        assert_eq!(r.status, 200);
        assert!(matches!(rx.try_recv(), Ok(RxEvent::PinShown { pin, .. }) if pin == "1234"));
        let body = plist_body(&[("method", Value::String("pin".into())), ("user", Value::String(user.into()))]);
        let r = s.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        assert_eq!(r.status, 200);
        let client = PinSetupClient::new(user, "1234", &reply_data(&r, "salt"), &reply_data(&r, "pk")).unwrap();
        let body = plist_body(&[
            ("pk", Value::Data(client.public_key().to_vec())),
            ("proof", Value::Data(client.proof().to_vec())),
        ]);
        let r = s.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        assert!(client.verify_server_proof(&reply_data(&r, "proof")));
        let (epk, tag) = client.seal_key(&phone.public_key().to_bytes());
        let body = plist_body(&[("epk", Value::Data(epk.to_vec())), ("authTag", Value::Data(tag.to_vec()))]);
        let r = s.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        assert_eq!(client.open_key(&reply_data(&r, "epk"), &reply_data(&r, "authTag")).unwrap(), ctx.public_key);
        assert!(matches!(rx.try_recv(), Ok(RxEvent::PinFinished { paired: true, .. })));
        assert_eq!(legacy_verify(&mut s, &ctx, &phone).await, 200);

        // A new connection of the same phone needs no code; SETUP waits for pair-verify.
        let mut s = session(&ctx);
        let r = s.handle(&request("SETUP", "rtsp://x/1", first_setup_body(user), Some(BINARY_PLIST))).await;
        assert_eq!(r.status, 470);
        assert_eq!(legacy_verify(&mut s, &ctx, &phone).await, 200);
        let r = s.handle(&request("SETUP", "rtsp://x/1", first_setup_body(user), Some(BINARY_PLIST))).await;
        assert_ne!(r.status, 470);

        // Another phone was never paired.
        let mut s = session(&ctx);
        assert_eq!(legacy_verify(&mut s, &ctx, &client_identity(4)).await, 470);
        std::fs::remove_dir_all(dir).ok();
    }

    // iOS asks for the code on one connection, closes it while the user types, and sends the
    // code on a new one: the random code shown first must still be the one checked.
    #[tokio::test]
    async fn random_pin_survives_the_connection_that_asked_for_it() {
        use rustplay_core::crypto::pin::PinSetupClient;
        let dir = std::env::temp_dir().join(format!("rustplay-pin-cross-{}", std::process::id()));
        let pin = crate::access::PinConfig { required: true, fixed: None };
        let state = crate::access::AccessState::new(pin, Some(dir.join("client-pairings.plist")));
        let (ctx, mut rx) = context_with(state);
        let user = "11:22:33:44:55:66";
        let shown = {
            let mut first = session(&ctx);
            first.handle(&request("POST", "/pair-pin-start", Vec::new(), None)).await;
            match rx.try_recv() {
                Ok(RxEvent::PinShown { pin, .. }) => pin,
                other => panic!("se esperaba el código: {other:?}"),
            }
        };
        let mut second = session(&ctx);
        let body = plist_body(&[("method", Value::String("pin".into())), ("user", Value::String(user.into()))]);
        let r = second.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        assert_eq!(r.status, 200);
        assert!(rx.try_recv().is_err(), "the code must not change on the second connection");
        let client = PinSetupClient::new(user, &shown, &reply_data(&r, "salt"), &reply_data(&r, "pk")).unwrap();
        let body = plist_body(&[
            ("pk", Value::Data(client.public_key().to_vec())),
            ("proof", Value::Data(client.proof().to_vec())),
        ]);
        let r = second.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        assert_eq!(r.status, 200);
        assert!(client.verify_server_proof(&reply_data(&r, "proof")));
        std::fs::remove_dir_all(dir).ok();
    }

    // Runs the SRP steps of /pair-setup-pin on a fresh connection with `code`; returns the status
    // of the proof step.
    async fn try_pin(ctx: &Context, code: &str) -> u16 {
        use rustplay_core::crypto::pin::PinSetupClient;
        let mut s = session(ctx);
        let body = plist_body(&[("method", Value::String("pin".into())), ("user", Value::String("X".into()))]);
        let r = s.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await;
        let client = PinSetupClient::new("X", code, &reply_data(&r, "salt"), &reply_data(&r, "pk")).unwrap();
        let body = plist_body(&[
            ("pk", Value::Data(client.public_key().to_vec())),
            ("proof", Value::Data(client.proof().to_vec())),
        ]);
        s.handle(&request("POST", "/pair-setup-pin", body, Some(BINARY_PLIST))).await.status
    }

    // iOS tries a code on its own before asking the user: that failure must keep the code on
    // screen and valid. Only repeated failures discard it.
    #[tokio::test]
    async fn wrong_pin_fails_with_470_but_keeps_the_code() {
        let dir = std::env::temp_dir().join(format!("rustplay-pin-bad-{}", std::process::id()));
        let pin = crate::access::PinConfig { required: true, fixed: None };
        let state = crate::access::AccessState::new(pin, Some(dir.join("client-pairings.plist")));
        let (ctx, mut rx) = context_with(state);
        session(&ctx).handle(&request("POST", "/pair-pin-start", Vec::new(), None)).await;
        let shown = match rx.try_recv() {
            Ok(RxEvent::PinShown { pin, .. }) => pin,
            other => panic!("se esperaba el código: {other:?}"),
        };
        let wrong = if shown == "0000" { "1111" } else { "0000" };
        assert_eq!(try_pin(&ctx, wrong).await, 470);
        assert!(rx.try_recv().is_err(), "a single failure neither closes nor changes the code");
        assert_eq!(try_pin(&ctx, &shown).await, 200);

        for _ in 0..5 {
            assert_eq!(try_pin(&ctx, wrong).await, 470);
        }
        let finished = std::iter::from_fn(|| rx.try_recv().ok()).any(|e| matches!(e, RxEvent::PinFinished { paired: false, .. }));
        assert!(finished, "the code is discarded after too many failures");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn blocked_device_gets_401_at_first_setup() {
        let (ctx, mut rx) = context();
        ctx.live.update(|c| c.access.blocked = vec!["aa:bb:cc:dd:ee:ff".into()]);
        let r = session(&ctx)
            .handle(&request("SETUP", "rtsp://x/1", first_setup_body("AA:BB:CC:DD:EE:FF"), Some(BINARY_PLIST)))
            .await;
        assert_eq!(r.status, 401);
        assert!(matches!(rx.try_recv(), Ok(RxEvent::DeviceSeen { name: Some(n), .. }) if n == "iPhone de Ana"));
        assert!(matches!(rx.try_recv(), Ok(RxEvent::AccessDenied { .. })));
    }

    #[tokio::test]
    async fn ask_waits_for_the_answer() {
        let (ctx, mut rx) = context();
        ctx.live.update(|c| c.access.policy = crate::AccessPolicy::Ask);
        for allow in [false, true] {
            let device = if allow { "AA:00:00:00:00:02" } else { "AA:00:00:00:00:01" };
            let mut s = session(&ctx);
            let setup = request("SETUP", "rtsp://x/1", first_setup_body(device), Some(BINARY_PLIST));
            let answer = async {
                loop {
                    match rx.recv().await {
                        Some(RxEvent::AccessRequest { id, device_id, .. }) => {
                            assert_eq!(device_id, device);
                            assert!(ctx.access.answer(id, allow));
                            return;
                        }
                        Some(_) => {}
                        None => panic!("canal cerrado"),
                    }
                }
            };
            let (r, ()) = tokio::join!(s.handle(&setup), answer);
            assert_eq!(r.status == 401, !allow, "permitir = {allow}");
        }
    }

    #[tokio::test]
    async fn teardown_without_streams_closes() {
        let (ctx, _rx) = context();
        let r = session(&ctx)
            .handle(&request("TEARDOWN", "rtsp://x/1", Vec::new(), None))
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("Connection"), Some("close"));
    }
}
