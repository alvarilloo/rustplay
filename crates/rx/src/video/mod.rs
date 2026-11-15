// AirPlay video ("casting"): the client sends a URL with POST /play instead of mirroring its
// screen and drives a player on this PC with /rate, /scrub, /stop and /playback-info. The YouTube
// app instead sends a private HLS location (mlhls://localhost/master.m3u8) and hands over the
// playlists one by one through FCUP requests on the reverse connection; we keep them and serve them
// to the local player over loopback from this same port.
mod hls;
mod request;
mod reverse;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use plist::{Dictionary, Value};
use rustplay_core::media::player::{PlaybackStatus, PlayerCommand, PlayerOptions, StartAt, VideoPlayer};
use rustplay_core::rtsp::{Request, Response};
use rustplay_core::Result;
use tokio::sync::mpsc;

use crate::RxEvent;
use request::Action;
pub use reverse::serve as serve_reverse;
pub(crate) use reverse::{event_reply, take_event};
use reverse::{XML_PLIST, xml};

const SESSION_HEADER: &str = "X-Apple-Session-ID";
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
// Without a reverse connection for this long, the client is gone and the video stops.
const UNLINKED_GRACE: Duration = Duration::from_secs(6);
const MAX_RESUME: usize = 8;
// Features reported by GET /server-info: video, photo, FairPlay video, volume, HLS, slideshow,
// (unknown) and audio, as UxPlay and older Apple TVs report them.
const SERVER_INFO_FEATURES: u64 = 0x27F;

pub trait Player: Send {
    fn status(&self) -> PlaybackStatus;
    fn command(&self, command: PlayerCommand);
}

impl Player for VideoPlayer {
    fn status(&self) -> PlaybackStatus {
        VideoPlayer::status(self)
    }

    fn command(&self, command: PlayerCommand) {
        VideoPlayer::command(self, command);
    }
}

pub struct PlayerRequest {
    pub url: String,
    pub start: StartAt,
    pub title: String,
}

pub type PlayerFactory = Arc<dyn Fn(PlayerRequest) -> Result<Box<dyn Player>> + Send + Sync>;

// Opens the real player window with the receiver's current window options and texts.
pub fn window_factory(live: Arc<crate::Live>) -> PlayerFactory {
    Arc::new(move |request: PlayerRequest| {
        let config = live.get();
        let options = PlayerOptions {
            title: request.title,
            fullscreen: config.window.fullscreen,
            always_on_top: config.window.always_on_top,
            controls: config.window.controls,
            texts: config.video_texts.clone(),
        };
        let player = VideoPlayer::open(request.url, request.start, options)?;
        Ok(Box::new(player) as Box<dyn Player>)
    })
}

struct Link {
    id: u64,
    session: String,
    tx: mpsc::UnboundedSender<Request>,
}

struct Media {
    uri: String,
    // `uri` without the client's prefix: what the local player asks for.
    path: String,
    playlist: Option<String>,
    done: bool,
}

struct Hls {
    prefix: String,
    master: Option<String>,
    media: Vec<Media>,
    next_request: u64,
    // Sent as soon as the client opens its reverse connection.
    pending: Option<Request>,
}

struct Playback {
    id: u64,
    session: String,
    uuid: Option<String>,
    peer: SocketAddr,
    start: StartAt,
    hls: Option<Hls>,
    player: Option<Box<dyn Player>>,
    reported: Option<&'static str>,
    linked: bool,
    unlinked_since: Option<Instant>,
}

#[derive(Default)]
struct State {
    links: Vec<Link>,
    next_link: u64,
    next_playback: u64,
    current: Option<Playback>,
    // Sessions whose video we ended (window closed, end of media): their next /playback-info
    // closes the connection so the client leaves its AirPlay mode.
    ended: Vec<String>,
    // Last positions of videos the client removed (YouTube does it to show an advert).
    resume: Vec<(String, f64)>,
    // Number of /playback-info requests, for the log.
    polls: u64,
}

pub struct Hub {
    enabled: bool,
    local_base: String,
    title: String,
    device_id: String,
    languages: Vec<String>,
    factory: PlayerFactory,
    events: mpsc::Sender<RxEvent>,
    state: Mutex<State>,
    // Linear volume the device asked for (as `f32` bits); it also applies to videos that start later.
    volume: std::sync::atomic::AtomicU32,
}

fn status(request: &Request, code: u16, reason: &'static str) -> Response {
    Response::new(request.protocol, code, reason)
}

fn bad_request(request: &Request) -> Response {
    status(request, 400, "Bad Request")
}

fn plist_xml(request: &Request, dict: Dictionary) -> Response {
    match xml(dict) {
        Ok(body) => Response::ok(request.protocol).with_body(XML_PLIST, body),
        Err(e) => {
            tracing::warn!("no se pudo construir el plist de vídeo: {e}");
            status(request, 500, "Internal Server Error")
        }
    }
}

fn closing(mut response: Response) -> Response {
    response.headers.set("Connection", "close");
    response
}

fn is_http(location: &str) -> bool {
    let lower = location.get(..8).unwrap_or(location).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn send_on(state: &State, session: &str, request: Request) -> std::result::Result<(), Request> {
    let link = state.links.iter().rev().find(|l| l.session == session).or_else(|| state.links.last());
    match link {
        Some(link) => link.tx.send(request).map_err(|e| e.0),
        None => Err(request),
    }
}

impl Hub {
    pub fn new(
        enabled: bool,
        port: u16,
        title: String,
        device_id: String,
        languages: Vec<String>,
        factory: PlayerFactory,
        events: mpsc::Sender<RxEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            local_base: format!("http://127.0.0.1:{port}"),
            title,
            device_id,
            languages,
            factory,
            events,
            state: Mutex::new(State::default()),
            volume: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
        })
    }

    #[cfg(test)]
    pub fn disabled(events: mpsc::Sender<RxEvent>) -> Arc<Self> {
        let factory: PlayerFactory = Arc::new(|_| Err(rustplay_core::Error::NotImplemented("reproductor de vídeo")));
        Self::new(false, 0, String::new(), String::new(), Vec::new(), factory, events)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: RxEvent) {
        let _ = self.events.try_send(event);
    }

    pub(crate) fn add_link(&self, session: &str, tx: mpsc::UnboundedSender<Request>) -> u64 {
        let mut state = self.lock();
        state.next_link += 1;
        let id = state.next_link;
        state.links.push(Link { id, session: session.to_owned(), tx });
        let pending = state.current.as_mut().and_then(|p| p.hls.as_mut()).and_then(|h| h.pending.take());
        if let Some(request) = pending {
            if let Err(request) = send_on(&state, session, request) {
                if let Some(hls) = state.current.as_mut().and_then(|p| p.hls.as_mut()) {
                    hls.pending = Some(request);
                }
            }
        }
        id
    }

    pub(crate) fn remove_link(&self, id: u64) {
        self.lock().links.retain(|l| l.id != id);
    }

    // Stops every video (receiver shutdown).
    pub fn stop_all(&self) {
        let mut state = self.lock();
        self.finish(&mut state, false);
    }

    // Handles the AirPlay video requests; None for anything else. The session ID comes back when
    // the connection must turn into a reverse connection (after the 101 response is written).
    pub fn handle(self: &Arc<Self>, request: &Request, peer: SocketAddr) -> Option<(Response, Option<String>)> {
        if !self.enabled {
            return None;
        }
        let uri = request.uri.as_str();
        let path = uri.split('?').next().unwrap_or(uri);
        // Every request about the video goes to the log (the polling of /playback-info only
        // every few seconds), to see what the device asks for before it gives up.
        if path != "/playback-info" && !(request.method == "GET" && self.is_local_player(request, peer)) {
            tracing::info!(%peer, "vídeo de app: {} {} ({} bytes)", request.method, uri, request.body.len());
        }
        let response = match (request.method.as_str(), path) {
            ("POST", "/reverse") => return Some(self.reverse(request, peer)),
            ("POST", "/play") => self.play(request, peer),
            ("GET", "/playback-info") => self.playback_info(request),
            ("POST", "/scrub") => self.scrub(request),
            ("GET", "/scrub") => self.scrub_info(request),
            ("POST", "/rate") => self.rate(request),
            ("POST", "/stop") => self.stop(request, peer),
            ("POST", "/action") => self.action(request, peer),
            ("PUT", "/setProperty") => self.set_property(request),
            ("GET" | "POST", "/getProperty") => {
                tracing::debug!(property = request::query(uri), "getProperty sin manejar");
                Response::ok(request.protocol)
            }
            ("GET", "/server-info") => self.server_info(request),
            ("POST", "/fp-setup2") => {
                tracing::warn!(%peer, "el cliente pide FairPlay para vídeo protegido, que no está admitido");
                status(request, 421, "Misdirected Request")
            }
            ("GET", p) if self.is_local_player(request, peer) => self.serve_playlist(request, p),
            _ => return None,
        };
        Some((response, None))
    }

    fn is_local_player(&self, request: &Request, peer: SocketAddr) -> bool {
        peer.ip().is_loopback()
            && request.cseq().is_none()
            && request.headers.get(SESSION_HEADER).is_none()
            && request.uri.contains("m3u8")
    }

    fn reverse(&self, request: &Request, peer: SocketAddr) -> (Response, Option<String>) {
        let session = request.headers.get(SESSION_HEADER).unwrap_or_default().to_owned();
        tracing::debug!(
            %peer,
            purpose = request.headers.get("X-Apple-Purpose").unwrap_or_default(),
            "el cliente pide un canal inverso"
        );
        let mut response = status(request, 101, "Switching Protocols");
        response.headers.set("Connection", "Upgrade");
        response.headers.set("Upgrade", "PTTH/1.0");
        (response, Some(session))
    }

    fn play(self: &Arc<Self>, request: &Request, peer: SocketAddr) -> Response {
        let Some(session) = request.headers.get(SESSION_HEADER).map(str::to_owned) else {
            tracing::warn!(%peer, "POST /play sin X-Apple-Session-ID");
            return bad_request(request);
        };
        let play = match request::parse_play(request.headers.get("Content-Type"), &request.body) {
            Ok(play) => play,
            Err(e) => {
                tracing::warn!(%peer, "POST /play no válido: {e}");
                return bad_request(request);
            }
        };
        tracing::info!(
            %peer,
            client = play.client.as_deref().unwrap_or("?"),
            "el dispositivo envía un vídeo: {}",
            play.location
        );
        let mut state = self.lock();
        self.finish(&mut state, false);
        let mut start = play.start;
        if let Some(uuid) = &play.uuid
            && let Some(pos) = state.resume.iter().position(|(u, _)| u == uuid)
        {
            let (_, resume) = state.resume.remove(pos);
            if matches!(start, StartAt::Seconds(s) if s < resume) || matches!(start, StartAt::Fraction(_)) {
                tracing::info!("se reanuda el vídeo en {resume:.1} s");
                start = StartAt::Seconds(resume);
            }
        }
        state.next_playback += 1;
        let mut playback = Playback {
            id: state.next_playback,
            session: session.clone(),
            uuid: play.uuid,
            peer,
            start,
            hls: None,
            player: None,
            reported: None,
            linked: false,
            unlinked_since: None,
        };
        if is_http(&play.location) {
            match self.open_player(play.location, start) {
                Ok(player) => playback.player = Some(player),
                Err(e) => {
                    tracing::warn!(%peer, "no se pudo abrir el reproductor de vídeo: {e}");
                    return status(request, 500, "Internal Server Error");
                }
            }
        } else if let Some(at) = play.location.find(hls::MASTER_PATH) {
            let mut hls = Hls {
                prefix: play.location[..at].to_owned(),
                master: None,
                media: Vec::new(),
                next_request: 1,
                pending: None,
            };
            match reverse::fcup_request(&play.location, hls.next_request, &session) {
                Ok(fcup) => {
                    hls.next_request += 1;
                    if let Err(fcup) = send_on(&state, &session, fcup) {
                        tracing::debug!("aún no hay canal inverso; la lista de reproducción se pedirá al abrirlo");
                        hls.pending = Some(fcup);
                    }
                }
                Err(e) => {
                    tracing::warn!("no se pudo construir la petición FCUP: {e}");
                    return status(request, 500, "Internal Server Error");
                }
            }
            playback.hls = Some(hls);
        } else {
            tracing::warn!(%peer, "ubicación de vídeo no admitida: {}", play.location);
            return bad_request(request);
        }
        let id = playback.id;
        state.current = Some(playback);
        drop(state);
        self.emit(RxEvent::VideoStarted { peer });
        tokio::spawn(watch(Arc::downgrade(self), id));
        Response::ok(request.protocol)
    }

    fn open_player(&self, url: String, start: StartAt) -> Result<Box<dyn Player>> {
        let player = (self.factory)(PlayerRequest { url, start, title: self.title.clone() })?;
        player.command(PlayerCommand::Volume(f32::from_bits(self.volume.load(std::sync::atomic::Ordering::Acquire))));
        Ok(player)
    }

    // The device's volume buttons: applies to the video being played and the next ones.
    pub fn set_volume(&self, linear: f32) {
        let linear = if linear.is_finite() { linear.clamp(0.0, 1.0) } else { 0.0 };
        self.volume.store(linear.to_bits(), std::sync::atomic::Ordering::Release);
        let state = self.lock();
        if let Some(player) = state.current.as_ref().and_then(|p| p.player.as_ref()) {
            player.command(PlayerCommand::Volume(linear));
        }
    }

    // Ends the current video. With `notify`, the client learns it through the reverse channel
    // and its next /playback-info.
    fn finish(&self, state: &mut State, notify: bool) {
        let Some(playback) = state.current.take() else { return };
        if let Some(player) = &playback.player {
            player.command(PlayerCommand::Stop);
        }
        if notify {
            match reverse::state_event("stopped", &playback.session) {
                Ok(event) => {
                    let _ = send_on(state, &playback.session, event);
                }
                Err(e) => tracing::debug!("no se pudo construir el evento de parada: {e}"),
            }
            state.ended.retain(|s| *s != playback.session);
            state.ended.push(playback.session.clone());
        }
        tracing::info!(peer = %playback.peer, "reproducción de vídeo terminada");
        self.emit(RxEvent::VideoStopped);
    }

    fn playback_info(&self, request: &Request) -> Response {
        let session = request.headers.get(SESSION_HEADER).unwrap_or_default();
        let mut state = self.lock();
        state.polls += 1;
        let polls = state.polls;
        if let Some(pos) = state.ended.iter().position(|s| s == session) {
            state.ended.remove(pos);
            return closing(Response::ok(request.protocol));
        }
        let Some(status) = state.current.as_ref().and_then(|p| p.player.as_ref()).map(|p| p.status()) else {
            return Response::ok(request.protocol);
        };
        drop(state);
        if !status.opened {
            return Response::ok(request.protocol);
        }
        let duration = if status.duration.is_finite() { status.duration.max(0.0) } else { 0.0 };
        let position = status.position.clamp(0.0, if duration > 0.0 { duration } else { f64::MAX });
        let range = |start: f64, length: f64| {
            let mut d = Dictionary::new();
            d.insert("duration".into(), Value::Real(length.max(0.0)));
            d.insert("start".into(), Value::Real(start));
            Value::Array(vec![Value::Dictionary(d)])
        };
        if polls % 5 == 1 {
            tracing::info!(
                "vídeo de app: /playback-info nº {polls}: posición {position:.1} s de {duration:.0} s, reproduciendo {}, cargando {}",
                status.playing,
                status.buffering
            );
        }
        let flag = |v: bool| Value::from(u64::from(v));
        let mut root = Dictionary::new();
        root.insert("duration".into(), Value::Real(duration));
        root.insert("position".into(), Value::Real(position));
        root.insert("rate".into(), Value::Real(if !status.playing { 0.0 } else if status.rate > 0.0 { status.rate } else { 1.0 }));
        root.insert("readyToPlay".into(), flag(true));
        root.insert("playbackBufferEmpty".into(), flag(status.buffering));
        root.insert("playbackBufferFull".into(), flag(!status.buffering));
        root.insert("playbackLikelyToKeepUp".into(), flag(!status.buffering));
        root.insert("loadedTimeRanges".into(), range(position, duration - position));
        root.insert("seekableTimeRanges".into(), range(0.0, duration));
        plist_xml(request, root)
    }

    fn command(&self, command: PlayerCommand) -> bool {
        let state = self.lock();
        match state.current.as_ref().and_then(|p| p.player.as_ref()) {
            Some(player) => {
                player.command(command);
                true
            }
            None => false,
        }
    }

    fn scrub(&self, request: &Request) -> Response {
        match request::query_number(&request.uri, "position") {
            Some(position) => {
                tracing::debug!("el dispositivo mueve el vídeo a {position:.1} s");
                self.command(PlayerCommand::Seek(position.max(0.0)));
            }
            None => tracing::debug!("scrub sin posición: {}", request.uri),
        }
        Response::ok(request.protocol)
    }

    fn scrub_info(&self, request: &Request) -> Response {
        let state = self.lock();
        let status = state.current.as_ref().and_then(|p| p.player.as_ref()).map(|p| p.status()).unwrap_or_default();
        let body = format!("duration: {:.6}\nposition: {:.6}\n", status.duration.max(0.0), status.position.max(0.0));
        Response::ok(request.protocol).with_body("text/parameters", Bytes::from(body))
    }

    fn rate(&self, request: &Request) -> Response {
        match request::query_number(&request.uri, "value") {
            Some(rate) if rate > 0.0 => {
                self.command(PlayerCommand::Play);
            }
            Some(_) => {
                self.command(PlayerCommand::Pause);
            }
            None => tracing::debug!("rate sin valor: {}", request.uri),
        }
        Response::ok(request.protocol)
    }

    fn stop(&self, request: &Request, peer: SocketAddr) -> Response {
        tracing::info!(%peer, "el dispositivo detiene el vídeo");
        let mut state = self.lock();
        self.finish(&mut state, false);
        Response::ok(request.protocol)
    }

    fn set_property(&self, request: &Request) -> Response {
        let property = request::query(&request.uri);
        const KNOWN: [&str; 7] = [
            "actionAtItemEnd",
            "selectedMediaArray",
            "reverseEndTime",
            "forwardEndTime",
            "interstitialEvents",
            "textMarkupArray",
            "isInterestedInDateRange",
        ];
        tracing::debug!(property, "setProperty");
        if KNOWN.contains(&property) {
            let mut root = Dictionary::new();
            root.insert("errorCode".into(), Value::from(0u64));
            return plist_xml(request, root);
        }
        Response::ok(request.protocol)
    }

    fn server_info(&self, request: &Request) -> Response {
        let mut root = Dictionary::new();
        root.insert("features".into(), Value::from(SERVER_INFO_FEATURES));
        root.insert("macAddress".into(), Value::String(self.device_id.clone()));
        root.insert("model".into(), Value::String(crate::txt::MODEL.into()));
        root.insert("osBuildVersion".into(), Value::String("12B435".into()));
        root.insert("protovers".into(), Value::String("1.0".into()));
        root.insert("srcvers".into(), Value::String(crate::txt::SOURCE_VERSION.into()));
        root.insert("vv".into(), Value::from(2u64));
        root.insert("deviceid".into(), Value::String(self.device_id.clone()));
        plist_xml(request, root)
    }

    fn action(&self, request: &Request, peer: SocketAddr) -> Response {
        let action = match request::parse_action(&request.body) {
            Ok(action) => action,
            Err(e) => {
                tracing::warn!(%peer, "POST /action no válido: {e}");
                return bad_request(request);
            }
        };
        match action {
            Action::FcupResponse { url, data, status: code, request_id } => {
                tracing::debug!(%peer, ?code, ?request_id, "lista de reproducción recibida: {url}");
                if self.store_playlist(&url, &data) {
                    Response::ok(request.protocol)
                } else {
                    bad_request(request)
                }
            }
            Action::PlaylistRemove { uuid } => {
                let mut state = self.lock();
                let current = state.current.as_ref().filter(|p| p.uuid.is_some() && p.uuid == uuid);
                if let Some(playback) = current {
                    let position = playback.player.as_ref().map_or(0.0, |p| p.status().position);
                    if let Some(uuid) = playback.uuid.clone() {
                        state.resume.retain(|(u, _)| *u != uuid);
                        state.resume.push((uuid, position));
                        if state.resume.len() > MAX_RESUME {
                            state.resume.remove(0);
                        }
                    }
                    tracing::info!(%peer, "el dispositivo retira el vídeo (se guarda la posición {position:.1} s)");
                    self.finish(&mut state, false);
                }
                Response::ok(request.protocol)
            }
            Action::PlaylistInsert => {
                tracing::debug!(%peer, "playlistInsert sin manejar");
                Response::ok(request.protocol)
            }
            Action::Other(kind) => {
                tracing::debug!(%peer, "acción de vídeo desconocida: {kind}");
                Response::ok(request.protocol)
            }
        }
    }

    // Keeps a playlist the client sent, then asks for the next one or starts the player.
    fn store_playlist(&self, url: &str, data: &[u8]) -> bool {
        let mut state = self.lock();
        let Some(playback) = state.current.as_mut() else {
            tracing::warn!("lista de reproducción recibida sin vídeo en curso");
            return false;
        };
        let session = playback.session.clone();
        let Some(hls) = playback.hls.as_mut() else {
            tracing::warn!("lista de reproducción recibida para un vídeo que no es HLS");
            return false;
        };
        let text = hls::normalize(data);
        let first_media = !hls.media.iter().any(|m| m.done);
        if url.contains(hls::MASTER_PATH) {
            let Some(master) = text else {
                tracing::warn!("la lista maestra recibida no es una lista M3U8");
                return false;
            };
            let master = hls::playable_only(&hls::select_audio_language(&master, &self.languages));
            let uris = hls::media_uris(&master, &hls.prefix);
            tracing::info!("lista maestra recibida con {} listas de medios", uris.len());
            // The start of the master shows the formats the app offers (diagnostics).
            let head: Vec<&str> = master.lines().take(14).collect();
            tracing::info!("inicio de la lista maestra:\n{}", head.join("\n"));
            hls.master = Some(hls::rewrite_master(&master, &hls.prefix, &self.local_base));
            hls.media = uris
                .into_iter()
                .map(|uri| Media { path: uri[hls.prefix.len()..].to_owned(), uri, playlist: None, done: false })
                .collect();
        } else if let Some(media) = hls.media.iter_mut().find(|m| m.uri == url) {
            media.done = true;
            match text {
                Some(text) => {
                    let expanded = hls::expand_condensed(&text).unwrap_or_else(|| {
                        tracing::warn!("lista de medios con YT-EXT-CONDENSED-URL no válida: {url}");
                        text
                    });
                    let (count, duration, complete) = hls::analyze_media(&expanded);
                    if first_media {
                        // Header and first segment of the first media playlist (diagnostics).
                        let mut head = Vec::new();
                        for line in expanded.lines() {
                            head.push(line.chars().take(300).collect::<String>());
                            if !line.starts_with('#') && !line.is_empty() {
                                break;
                            }
                        }
                        tracing::info!("inicio de la lista de medios {url}:\n{}", head.join("\n"));
                    }
                    tracing::debug!("lista de medios con {count} segmentos, {duration:.1} s (completa: {complete})");
                    media.playlist = Some(expanded);
                }
                None => tracing::warn!("la lista de medios recibida no es una lista M3U8: {url}"),
            }
        } else {
            tracing::warn!("lista de reproducción inesperada: {url}");
            return true;
        }
        let next = hls.media.iter().find(|m| !m.done).map(|m| m.uri.clone());
        if let Some(uri) = next {
            let id = hls.next_request;
            hls.next_request += 1;
            match reverse::fcup_request(&uri, id, &session) {
                Ok(fcup) => {
                    if let Err(fcup) = send_on(&state, &session, fcup)
                        && let Some(hls) = state.current.as_mut().and_then(|p| p.hls.as_mut())
                    {
                        hls.pending = Some(fcup);
                    }
                }
                Err(e) => tracing::warn!("no se pudo construir la petición FCUP: {e}"),
            }
            return true;
        }
        if playback.player.is_none() {
            let url = format!("{}{}", self.local_base, hls::MASTER_PATH);
            let start = playback.start;
            match self.open_player(url, start) {
                Ok(player) => playback.player = Some(player),
                Err(e) => {
                    tracing::warn!("no se pudo abrir el reproductor de vídeo: {e}");
                    self.finish(&mut state, true);
                }
            }
        }
        true
    }

    fn serve_playlist(&self, request: &Request, path: &str) -> Response {
        let state = self.lock();
        let hls = state.current.as_ref().and_then(|p| p.hls.as_ref());
        let body = hls.and_then(|hls| {
            if path == hls::MASTER_PATH {
                hls.master.clone()
            } else {
                hls.media
                    .iter()
                    .find(|m| m.path == path || m.path == request.uri || m.uri.ends_with(&request.uri))
                    .and_then(|m| m.playlist.clone())
            }
        });
        drop(state);
        match body {
            Some(body) => {
                let mut response =
                    Response::ok(request.protocol).with_body("application/x-mpegURL; charset=utf-8", Bytes::from(body));
                response.headers.set("Access-Control-Allow-Origin", "*");
                response.headers.set("Cache-Control", "no-cache");
                response
            }
            None => {
                tracing::debug!("el reproductor pide una lista que no existe: {}", request.uri);
                status(request, 404, "Not Found")
            }
        }
    }

    // Follows the player of video `id`: reports play/pause changes to the client and ends the
    // video when the window is closed, the media ends or fails, or the client goes away.
    fn tick(&self, id: u64) -> bool {
        let mut state = self.lock();
        let has_link = |state: &State, session: &str| state.links.iter().any(|l| l.session == session);
        let Some(playback) = state.current.as_ref().filter(|p| p.id == id) else { return false };
        let session = playback.session.clone();
        let linked_now = has_link(&state, &session);
        let status = playback.player.as_ref().map(|p| p.status());
        if let Some(status) = status {
            if status.closed || status.ended || status.failed {
                tracing::info!(
                    peer = %playback.peer,
                    "el vídeo ha terminado ({})",
                    if status.failed { "error" } else if status.ended { "fin" } else { "ventana cerrada" }
                );
                self.finish(&mut state, true);
                return false;
            }
        }
        let Some(playback) = state.current.as_mut() else { return false };
        if linked_now {
            playback.linked = true;
            playback.unlinked_since = None;
        } else if playback.linked {
            let since = *playback.unlinked_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= UNLINKED_GRACE {
                tracing::info!(peer = %playback.peer, "el dispositivo ha cerrado su canal inverso; se detiene el vídeo");
                self.finish(&mut state, false);
                return false;
            }
        }
        if let Some(status) = status.filter(|s| s.opened) {
            let now = if status.playing { "playing" } else { "paused" };
            if playback.reported != Some(now) {
                playback.reported = Some(now);
                match reverse::state_event(now, &session) {
                    Ok(event) => {
                        let _ = send_on(&state, &session, event);
                    }
                    Err(e) => tracing::debug!("no se pudo construir el evento de estado: {e}"),
                }
            }
        }
        true
    }
}

async fn watch(hub: Weak<Hub>, id: u64) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let Some(hub) = hub.upgrade() else { return };
        if !hub.tick(id) {
            return;
        }
    }
}
