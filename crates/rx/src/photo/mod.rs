// AirPlay photos and slideshows: the Photos app of an iPhone or iPad sends a picture with
// `PUT /photo` (JPEG body; `X-Apple-AssetAction: cacheOnly` first to preload the next one, then
// `displayCached` to show it) or a whole slideshow: `PUT /slideshows/1` with the settings
// (duration, theme, state) while we pull the pictures with `GET /slideshows/1/assets/<n>` over
// the reverse connection the client opens for it. One viewer window shows everything.
mod request;
mod reverse;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use plist::{Dictionary, Value};
use rustplay_core::media::photo::{self, DISSOLVE_SECONDS, PhotoOptions, PhotoViewer, Picture, Show, Transition};
use rustplay_core::rtsp::{Headers, Protocol, Request, Response};
use rustplay_core::Result;
use tokio::sync::mpsc;

use crate::RxEvent;
use request::{Action, PhotoRequest, Slideshow, State as PlayState};
pub use reverse::serve as serve_reverse;

const SESSION_HEADER: &str = "X-Apple-Session-ID";
const XML_PLIST: &str = "text/x-apple-plist+xml";
const TICK: Duration = Duration::from_millis(200);
// Same feature word as the video server-info: video, photo, FairPlay video, volume, HLS, slideshow...
const SERVER_INFO_FEATURES: u64 = 0x27F;
// Decoded pictures kept for `displayCached` (a decoded 12 MP photo is about 48 MB).
const MAX_CACHED: usize = 4;
const MAX_ASSETS: usize = 200;
const MAX_ASSET_BYTES: usize = 384 * 1024 * 1024;
// Themes offered in GET /slideshowFeatures; the rest are shown as the nearest of these.
const THEMES: [(&str, &str); 3] = [("Classic", "Classic"), ("Dissolve", "Dissolve"), ("KenBurns", "Ken Burns")];

pub trait Viewer: Send {
    fn show(&self, show: Show);
    fn closed(&self) -> bool;
    fn close(&self);
}

impl Viewer for PhotoViewer {
    fn show(&self, show: Show) {
        PhotoViewer::show(self, show);
    }

    fn closed(&self) -> bool {
        PhotoViewer::closed(self)
    }

    fn close(&self) {
        PhotoViewer::close(self);
    }
}

pub type ViewerFactory = Arc<dyn Fn(String) -> Result<Box<dyn Viewer>> + Send + Sync>;

// Opens the real viewer window with the receiver's current window options and texts.
pub fn window_factory(live: Arc<crate::Live>) -> ViewerFactory {
    Arc::new(move |title: String| {
        let config = live.get();
        let options = PhotoOptions {
            title,
            fullscreen: config.window.fullscreen,
            always_on_top: config.window.always_on_top,
            controls: config.window.controls,
            texts: config.video_texts.clone(),
        };
        Ok(Box::new(PhotoViewer::open(options)?) as Box<dyn Viewer>)
    })
}

struct Link {
    id: u64,
    session: String,
    tx: mpsc::UnboundedSender<Request>,
}

struct SlideshowState {
    settings: Slideshow,
    playing: bool,
    // Encoded pictures, in order.
    assets: Vec<Arc<Vec<u8>>>,
    bytes: usize,
    decoded: Vec<(usize, Arc<Picture>)>,
    current: Option<usize>,
    since: Instant,
    outstanding: bool,
    complete: bool,
}

#[derive(Default)]
struct Inner {
    viewer: Option<Box<dyn Viewer>>,
    generation: u64,
    peer: Option<SocketAddr>,
    session: Option<String>,
    photos: Vec<(String, Arc<Picture>)>,
    slideshow: Option<SlideshowState>,
    // Sessions whose window the user closed: their next request closes the connection.
    ended: Vec<String>,
    links: Vec<Link>,
    next_link: u64,
}

pub struct Hub {
    enabled: bool,
    title: String,
    device_id: String,
    factory: ViewerFactory,
    events: mpsc::Sender<RxEvent>,
    state: Mutex<Inner>,
    // Serialises the opening of the window.
    opening: tokio::sync::Mutex<()>,
}

fn status(request: &Request, code: u16, reason: &'static str) -> Response {
    Response::new(request.protocol, code, reason)
}

fn plist_xml(request: &Request, dict: Dictionary) -> Response {
    let mut out = Vec::new();
    match plist::to_writer_xml(&mut out, &Value::Dictionary(dict)) {
        Ok(()) => Response::ok(request.protocol).with_body(XML_PLIST, Bytes::from(out)),
        Err(e) => {
            tracing::warn!("no se pudo construir el plist de fotos: {e}");
            status(request, 500, "Internal Server Error")
        }
    }
}

fn closing(mut response: Response) -> Response {
    response.headers.set("Connection", "close");
    response
}

fn session_of(request: &Request, peer: SocketAddr) -> String {
    request.headers.get(SESSION_HEADER).map_or_else(|| peer.ip().to_string(), str::to_owned)
}

fn send_on(inner: &Inner, session: &str, request: Request) -> std::result::Result<(), Request> {
    let link = inner.links.iter().rev().find(|l| l.session == session).or_else(|| inner.links.last());
    match link {
        Some(link) => link.tx.send(request).map_err(|e| e.0),
        None => Err(request),
    }
}

async fn decode_blocking(bytes: Arc<Vec<u8>>) -> Result<Arc<Picture>> {
    tokio::task::spawn_blocking(move || photo::decode(&bytes))
        .await
        .map_err(|e| rustplay_core::Error::Media(format!("la decodificación de la foto falló: {e}")))?
        .map(Arc::new)
}

impl Hub {
    pub fn new(enabled: bool, title: String, device_id: String, factory: ViewerFactory, events: mpsc::Sender<RxEvent>) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            title,
            device_id,
            factory,
            events,
            state: Mutex::new(Inner::default()),
            opening: tokio::sync::Mutex::new(()),
        })
    }

    #[cfg(test)]
    pub fn disabled(events: mpsc::Sender<RxEvent>) -> Arc<Self> {
        let factory: ViewerFactory = Arc::new(|_| Err(rustplay_core::Error::NotImplemented("visor de fotos")));
        Self::new(false, String::new(), String::new(), factory, events)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: RxEvent) {
        let _ = self.events.try_send(event);
    }

    pub(crate) fn add_link(&self, session: &str, tx: mpsc::UnboundedSender<Request>) -> u64 {
        let mut inner = self.lock();
        inner.next_link += 1;
        let id = inner.next_link;
        inner.links.push(Link { id, session: session.to_owned(), tx });
        Self::request_asset(&mut inner, session);
        id
    }

    pub(crate) fn remove_link(&self, id: u64) {
        self.lock().links.retain(|l| l.id != id);
    }

    // Closes the window and forgets everything (receiver shutdown).
    pub fn stop_all(&self) {
        self.finish(false);
    }

    // Handles the photo requests; None for anything else. The session ID comes back when the
    // connection must turn into the slideshow reverse connection.
    pub async fn handle(self: &Arc<Self>, request: &Request, peer: SocketAddr, video_enabled: bool) -> Option<(Response, Option<String>)> {
        if !self.enabled {
            return None;
        }
        let uri = request.uri.as_str();
        let path = uri.split('?').next().unwrap_or(uri);
        let method = request.method.as_str();
        let purpose = request.headers.get("X-Apple-Purpose").unwrap_or_default();
        // With videos on, the video hub owns the event reverse connection.
        let reverse = method == "POST" && path == "/reverse" && (purpose.eq_ignore_ascii_case("slideshow") || !video_enabled);
        let ours = reverse
            || matches!(
                (method, path),
                ("GET", "/server-info" | "/slideshowFeatures") | ("PUT", "/photo" | "/slideshows/1") | ("DELETE", "/slideshows/1")
            )
            || (method == "PUT" && request::asset_number(path).is_some());
        let stopping = method == "POST" && path == "/stop" && self.active();
        if !ours && !stopping {
            return None;
        }
        tracing::info!(%peer, "foto de app: {method} {uri} ({} bytes)", request.body.len());
        let session = session_of(request, peer);
        if reverse {
            return Some(self.reverse(request, session, peer));
        }
        if matches!((method, path), ("PUT", "/photo" | "/slideshows/1")) && self.lock().ended.contains(&session) {
            tracing::info!(%peer, "la ventana de fotos se cerró; se cierra la conexión del dispositivo");
            return Some((closing(status(request, 403, "Forbidden")), None));
        }
        let response = match (method, path) {
            ("GET", "/server-info") => self.server_info(request),
            ("GET", "/slideshowFeatures") => Self::slideshow_features(request),
            ("PUT", "/photo") => self.put_photo(request, peer, session).await,
            ("PUT", "/slideshows/1") => self.put_slideshow(request, peer, session).await,
            ("PUT", p) => self.put_asset(request, p),
            ("DELETE" | "POST", _) => {
                self.finish(false);
                Response::ok(request.protocol)
            }
            _ => return None,
        };
        Some((response, None))
    }

    fn active(&self) -> bool {
        let inner = self.lock();
        inner.viewer.is_some() || inner.slideshow.is_some()
    }

    fn reverse(&self, request: &Request, session: String, peer: SocketAddr) -> (Response, Option<String>) {
        tracing::debug!(%peer, purpose = request.headers.get("X-Apple-Purpose").unwrap_or_default(), "el cliente pide un canal inverso para fotos");
        let mut response = status(request, 101, "Switching Protocols");
        response.headers.set("Connection", "Upgrade");
        response.headers.set("Upgrade", "PTTH/1.0");
        (response, Some(session))
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

    fn slideshow_features(request: &Request) -> Response {
        let themes = THEMES
            .iter()
            .map(|(key, name)| {
                let mut theme = Dictionary::new();
                theme.insert("key".into(), Value::String((*key).into()));
                theme.insert("name".into(), Value::String((*name).into()));
                Value::Dictionary(theme)
            })
            .collect();
        let mut root = Dictionary::new();
        root.insert("themes".into(), Value::Array(themes));
        plist_xml(request, root)
    }

    // Opens the window if it is not open; false when that failed.
    async fn ensure_viewer(self: &Arc<Self>, peer: SocketAddr, session: &str) -> bool {
        let _opening = self.opening.lock().await;
        {
            let mut inner = self.lock();
            if inner.viewer.as_ref().is_some_and(|v| !v.closed()) {
                inner.session = Some(session.to_owned());
                return true;
            }
        }
        let factory = Arc::clone(&self.factory);
        let title = self.title.clone();
        let opened = tokio::task::spawn_blocking(move || factory(title)).await;
        let viewer = match opened {
            Ok(Ok(viewer)) => viewer,
            Ok(Err(e)) => {
                tracing::warn!(%peer, "no se pudo abrir el visor de fotos: {e}");
                return false;
            }
            Err(e) => {
                tracing::warn!(%peer, "no se pudo abrir el visor de fotos: {e}");
                return false;
            }
        };
        let generation = {
            let mut inner = self.lock();
            inner.generation += 1;
            inner.viewer = Some(viewer);
            inner.peer = Some(peer);
            inner.session = Some(session.to_owned());
            inner.generation
        };
        self.emit(RxEvent::PhotosStarted { peer });
        tokio::spawn(watch(Arc::downgrade(self), generation));
        true
    }

    fn show_now(&self, picture: Arc<Picture>, fade: bool, ken_burns: Option<Duration>) {
        let inner = self.lock();
        if let Some(viewer) = &inner.viewer {
            let transition = if fade { Transition::Dissolve } else { Transition::None };
            viewer.show(Show { picture, transition, ken_burns });
        }
    }

    fn cache(&self, key: &str, picture: &Arc<Picture>) {
        let mut inner = self.lock();
        inner.photos.retain(|(k, _)| k != key);
        inner.photos.push((key.to_owned(), Arc::clone(picture)));
        if inner.photos.len() > MAX_CACHED {
            inner.photos.remove(0);
        }
    }

    async fn put_photo(self: &Arc<Self>, request: &Request, peer: SocketAddr, session: String) -> Response {
        let PhotoRequest { key, action, fade } = request::parse_photo(request);
        let picture = if request.body.is_empty() {
            let cached = key.as_deref().and_then(|k| self.lock().photos.iter().find(|(c, _)| c == k).map(|(_, p)| Arc::clone(p)));
            match (action, cached) {
                (Action::DisplayCached, Some(picture)) => picture,
                (Action::DisplayCached, None) => {
                    tracing::info!(%peer, "el dispositivo pide una foto que no está en la caché");
                    return status(request, 412, "Precondition Failed");
                }
                _ => return status(request, 400, "Bad Request"),
            }
        } else {
            match decode_blocking(Arc::new(request.body.to_vec())).await {
                Ok(picture) => {
                    if let Some(key) = &key {
                        self.cache(key, &picture);
                    }
                    picture
                }
                Err(e) => {
                    tracing::warn!(%peer, "no se pudo decodificar la foto ({} bytes): {e}", request.body.len());
                    return status(request, 415, "Unsupported Media Type");
                }
            }
        };
        if action == Action::CacheOnly {
            return Response::ok(request.protocol);
        }
        // A single photo replaces any slideshow.
        self.lock().slideshow = None;
        if !self.ensure_viewer(peer, &session).await {
            return status(request, 500, "Internal Server Error");
        }
        self.show_now(picture, fade, None);
        Response::ok(request.protocol)
    }

    async fn put_slideshow(self: &Arc<Self>, request: &Request, peer: SocketAddr, session: String) -> Response {
        let Some(settings) = request::parse_slideshow(&request.body) else {
            tracing::warn!(%peer, "PUT /slideshows/1 no válido");
            return status(request, 400, "Bad Request");
        };
        tracing::info!(%peer, "presentación: {} s por foto, tema {}, estado {:?}", settings.duration.as_secs(), settings.theme, settings.state);
        if settings.state == Some(PlayState::Stopped) {
            self.finish(false);
            return plist_xml(request, Dictionary::new());
        }
        if !self.ensure_viewer(peer, &session).await {
            return status(request, 500, "Internal Server Error");
        }
        {
            let mut inner = self.lock();
            let playing = settings.state != Some(PlayState::Paused);
            let show = inner.slideshow.get_or_insert_with(|| SlideshowState::new(settings.clone()));
            show.settings = settings;
            show.playing = playing;
            Self::request_asset(&mut inner, &session);
        }
        plist_xml(request, Dictionary::new())
    }

    // An asset the client pushed (`PUT /slideshows/1/assets/<n>`).
    fn put_asset(&self, request: &Request, path: &str) -> Response {
        let (Some(number), Some(bytes)) = (request::asset_number(path), request::asset_bytes(&request.body)) else {
            return status(request, 400, "Bad Request");
        };
        let mut inner = self.lock();
        let default = Slideshow { duration: request::DEFAULT_SLIDE, theme: "Classic".into(), state: None };
        let show = inner.slideshow.get_or_insert_with(|| SlideshowState::new(default));
        if !show.store(number as usize - 1, bytes) {
            return status(request, 413, "Payload Too Large");
        }
        Response::ok(request.protocol)
    }

    // Asks the client for the next slideshow picture, one at a time.
    fn request_asset(inner: &mut Inner, session: &str) {
        let Some(show) = inner.slideshow.as_mut() else { return };
        if show.outstanding || show.complete || show.assets.len() >= MAX_ASSETS || show.bytes >= MAX_ASSET_BYTES {
            return;
        }
        let id = show.assets.len() + 1;
        let mut headers = Headers::default();
        headers.set(SESSION_HEADER, session);
        let asset = Request {
            method: "GET".into(),
            uri: format!("/slideshows/1/assets/{id}"),
            protocol: Protocol::Http11,
            headers,
            body: Bytes::new(),
        };
        if send_on(inner, session, asset).is_ok()
            && let Some(show) = inner.slideshow.as_mut()
        {
            show.outstanding = true;
        }
    }

    // The client answered one of our asset requests on the reverse connection.
    pub(crate) fn asset_response(&self, response: &Response) {
        let mut inner = self.lock();
        let session = inner.session.clone().unwrap_or_default();
        let Some(show) = inner.slideshow.as_mut() else { return };
        show.outstanding = false;
        let bytes = (response.status == 200).then(|| request::asset_bytes(&response.body)).flatten();
        let stored = bytes.is_some_and(|bytes| show.store(show.assets.len(), bytes));
        if !stored {
            tracing::debug!(status = response.status, "fin de las fotos de la presentación ({} recibidas)", show.assets.len());
            show.complete = true;
        }
        Self::request_asset(&mut inner, &session);
    }

    // Closes the window and forgets the pictures. With `by_user` the device learns about it on
    // its next photo request.
    fn finish(&self, by_user: bool) {
        let mut inner = self.lock();
        let had_viewer = inner.viewer.is_some();
        if let Some(viewer) = inner.viewer.take() {
            viewer.close();
        }
        if by_user && let Some(session) = inner.session.clone() {
            inner.ended.retain(|s| *s != session);
            inner.ended.push(session);
            if inner.ended.len() > 8 {
                inner.ended.remove(0);
            }
        }
        inner.slideshow = None;
        inner.photos.clear();
        inner.generation += 1;
        drop(inner);
        if had_viewer {
            tracing::info!("visor de fotos cerrado");
            self.emit(RxEvent::PhotosStopped);
        }
    }

    // One pass of the watcher: what the slideshow should do now.
    fn plan(&self, generation: u64) -> Plan {
        let mut inner = self.lock();
        if inner.generation != generation {
            return Plan::Gone;
        }
        if inner.viewer.as_ref().is_none_or(|v| v.closed()) {
            return Plan::Closed;
        }
        let Some(show) = inner.slideshow.as_mut() else { return Plan::Idle };
        if !show.playing || show.assets.is_empty() {
            return Plan::Idle;
        }
        let due = match show.current {
            None => true,
            Some(_) => show.since.elapsed() >= show.settings.duration,
        };
        if !due {
            return Plan::Idle;
        }
        let len = show.assets.len();
        let index = match show.current {
            None => 0,
            Some(i) if i + 1 < len => i + 1,
            Some(_) if show.complete => 0,
            // The next picture has not arrived yet.
            Some(_) => return Plan::Idle,
        };
        let cached = show.decoded.iter().find(|(i, _)| *i == index).map(|(_, p)| Arc::clone(p));
        Plan::Advance { index, bytes: Arc::clone(&show.assets[index]), cached }
    }

    fn advanced(&self, generation: u64, index: usize, picture: Option<Arc<Picture>>) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.generation != generation {
            return;
        }
        let Some(show) = inner.slideshow.as_mut() else { return };
        show.current = Some(index);
        show.since = Instant::now();
        let Some(picture) = picture else { return };
        show.keep(index, Arc::clone(&picture));
        let ken_burns = show.settings.ken_burns().then(|| show.settings.duration + Duration::from_secs_f32(DISSOLVE_SECONDS));
        if let Some(viewer) = &inner.viewer {
            viewer.show(Show { picture, transition: Transition::Dissolve, ken_burns });
        }
    }

    // Decodes the picture after `index` in advance.
    fn preload_plan(&self, generation: u64, index: usize) -> Option<(usize, Arc<Vec<u8>>)> {
        let inner = self.lock();
        if inner.generation != generation {
            return None;
        }
        let show = inner.slideshow.as_ref()?;
        let len = show.assets.len();
        let next = if index + 1 < len { index + 1 } else if show.complete { 0 } else { return None };
        (len > 1 && !show.decoded.iter().any(|(i, _)| *i == next)).then(|| (next, Arc::clone(&show.assets[next])))
    }

    fn store_preloaded(&self, generation: u64, index: usize, picture: Arc<Picture>) {
        let mut inner = self.lock();
        if inner.generation == generation && let Some(show) = inner.slideshow.as_mut() {
            show.keep(index, picture);
        }
    }

    fn on_closed(&self, generation: u64) {
        if self.lock().generation == generation {
            tracing::info!("el usuario cerró la ventana de fotos");
            self.finish(true);
        }
    }
}

enum Plan {
    Gone,
    Closed,
    Idle,
    Advance { index: usize, bytes: Arc<Vec<u8>>, cached: Option<Arc<Picture>> },
}

impl SlideshowState {
    fn new(settings: Slideshow) -> Self {
        Self {
            settings,
            playing: false,
            assets: Vec::new(),
            bytes: 0,
            decoded: Vec::new(),
            current: None,
            since: Instant::now(),
            outstanding: false,
            complete: false,
        }
    }

    // Stores picture `index` (an existing slot is replaced, a new one must be the next).
    fn store(&mut self, index: usize, bytes: Vec<u8>) -> bool {
        if self.bytes + bytes.len() > MAX_ASSET_BYTES || self.assets.len() >= MAX_ASSETS && index >= self.assets.len() {
            return false;
        }
        self.bytes += bytes.len();
        let bytes = Arc::new(bytes);
        if index < self.assets.len() {
            self.bytes -= self.assets[index].len();
            self.assets[index] = bytes;
            self.decoded.retain(|(i, _)| *i != index);
        } else {
            self.assets.push(bytes);
        }
        true
    }

    // Keeps the decoded picture (two at most: the current one and the next).
    fn keep(&mut self, index: usize, picture: Arc<Picture>) {
        self.decoded.retain(|(i, _)| *i != index);
        self.decoded.push((index, picture));
        while self.decoded.len() > 2 {
            self.decoded.remove(0);
        }
    }
}

// Follows the window of one viewer: ends the session when the user closes it and moves the
// slideshow forward.
async fn watch(hub: Weak<Hub>, generation: u64) {
    loop {
        tokio::time::sleep(TICK).await;
        let Some(hub) = hub.upgrade() else { return };
        match hub.plan(generation) {
            Plan::Gone => return,
            Plan::Closed => {
                hub.on_closed(generation);
                return;
            }
            Plan::Idle => {}
            Plan::Advance { index, bytes, cached } => {
                let picture = match cached {
                    Some(picture) => Some(picture),
                    None => match decode_blocking(bytes).await {
                        Ok(picture) => Some(picture),
                        Err(e) => {
                            tracing::warn!("no se pudo decodificar una foto de la presentación: {e}");
                            None
                        }
                    },
                };
                hub.advanced(generation, index, picture);
                if let Some((next, bytes)) = hub.preload_plan(generation, index)
                    && let Ok(picture) = decode_blocking(bytes).await
                {
                    hub.store_preloaded(generation, next, picture);
                }
            }
        }
    }
}
