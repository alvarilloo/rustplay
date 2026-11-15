// Loopback tests of the AirPlay photo handlers: a tiny server drives the Hub with the real HTTP
// codec, and a fake viewer stands in for the window. Everything binds to 127.0.0.1.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use plist::{Dictionary, Value};
use rustplay_core::rtsp::{ClientCodec, Headers, Protocol, Request, Response, ServerCodec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use super::*;

type Shown = (u32, u32, Transition, Option<Duration>);

#[derive(Default)]
struct Probe {
    opened: Mutex<u32>,
    shown: Mutex<Vec<Shown>>,
    closed: AtomicBool,
    fail_open: AtomicBool,
}

struct FakeViewer(Arc<Probe>);

impl Viewer for FakeViewer {
    fn show(&self, show: Show) {
        self.0.shown.lock().unwrap().push((show.picture.width, show.picture.height, show.transition, show.ken_burns));
    }

    fn closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
    }
}

fn factory(probe: &Arc<Probe>) -> ViewerFactory {
    let probe = Arc::clone(probe);
    Arc::new(move |_title| {
        if probe.fail_open.load(Ordering::Acquire) {
            return Err(rustplay_core::Error::NotImplemented("visor de fotos"));
        }
        *probe.opened.lock().unwrap() += 1;
        probe.closed.store(false, Ordering::Release);
        Ok(Box::new(FakeViewer(Arc::clone(&probe))) as Box<dyn Viewer>)
    })
}

struct Rig {
    hub: Arc<Hub>,
    probe: Arc<Probe>,
    addr: std::net::SocketAddr,
    events: mpsc::Receiver<RxEvent>,
    cancel: CancellationToken,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn rig(enabled: bool, video_enabled: bool) -> Rig {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let probe = Arc::new(Probe::default());
    let (events_tx, events) = mpsc::channel(64);
    let hub = Hub::new(enabled, "PC".into(), "AA:BB:CC:DD:EE:FF".into(), factory(&probe), events_tx);
    let cancel = CancellationToken::new();
    let (server_hub, server_cancel) = (Arc::clone(&hub), cancel.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { return };
            tokio::spawn(serve(stream, peer, Arc::clone(&server_hub), video_enabled, server_cancel.clone()));
        }
    });
    Rig { hub, probe, addr, events, cancel }
}

// What rustplay-rx's connection loop does with the hub, minus pairing and RTSP.
async fn serve(mut stream: TcpStream, peer: std::net::SocketAddr, hub: Arc<Hub>, video_enabled: bool, cancel: CancellationToken) {
    let mut codec = ServerCodec;
    let mut inbound = BytesMut::new();
    let mut out = BytesMut::new();
    loop {
        while let Ok(Some(request)) = codec.decode(&mut inbound) {
            let (response, reverse) = hub
                .handle(&request, peer, video_enabled)
                .await
                .unwrap_or_else(|| (Response::new(request.protocol, 404, "Not Found"), None));
            let close = response.headers.get("Connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
            out.clear();
            codec.encode(response, &mut out).unwrap();
            stream.write_all(&out).await.unwrap();
            if close {
                return;
            }
            if let Some(session) = reverse {
                let _ = reverse::serve(&mut stream, inbound, hub, session, peer, &cancel).await;
                return;
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            read = stream.read_buf(&mut inbound) => if read.unwrap_or(0) == 0 { return },
        }
    }
}

struct Client {
    stream: TcpStream,
    inbound: BytesMut,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        Self { stream: TcpStream::connect(addr).await.unwrap(), inbound: BytesMut::new() }
    }

    async fn send(&mut self, method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) {
        let mut h = Headers::default();
        for (k, v) in headers {
            h.set(*k, *v);
        }
        let request = Request { method: method.into(), uri: uri.into(), protocol: Protocol::Http11, headers: h, body: Bytes::from(body) };
        let mut out = BytesMut::new();
        ClientCodec.encode(request, &mut out).unwrap();
        self.stream.write_all(&out).await.unwrap();
    }

    async fn response(&mut self) -> Response {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(response) = ClientCodec.decode(&mut self.inbound).unwrap() {
                    return response;
                }
                assert!(self.stream.read_buf(&mut self.inbound).await.unwrap() > 0, "connection closed");
            }
        })
        .await
        .expect("timed out waiting for a response")
    }

    async fn request(&mut self, method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Response {
        self.send(method, uri, headers, body).await;
        self.response().await
    }

    // A request the receiver sent on this (reverse) connection.
    async fn incoming(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(request) = ServerCodec.decode(&mut self.inbound).unwrap() {
                    return request;
                }
                assert!(self.stream.read_buf(&mut self.inbound).await.unwrap() > 0, "connection closed");
            }
        })
        .await
        .expect("timed out waiting for a request")
    }

    // The answer a client gives to the receiver's asset request.
    async fn answer(&mut self, status: u16, reason: &'static str, body: Vec<u8>) {
        let mut response = Response::new(Protocol::Http11, status, reason);
        response.body = Bytes::from(body);
        let mut out = BytesMut::new();
        ServerCodec.encode(response, &mut out).unwrap();
        self.stream.write_all(&out).await.unwrap();
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

// A valid w x h RGB PNG (uncompressed deflate blocks), so the tests need no encoder.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&[(x * 40) as u8, (y * 40) as u8, 90]);
        }
    }
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(60_000).peekable();
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut header = Vec::new();
    header.extend_from_slice(&w.to_be_bytes());
    header.extend_from_slice(&h.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn slideshow_body(seconds: i64, theme: &str, state: &str) -> Vec<u8> {
    let mut settings = Dictionary::new();
    settings.insert("slideDuration".into(), Value::Integer(seconds.into()));
    settings.insert("theme".into(), Value::String(theme.into()));
    settings.insert("state".into(), Value::String(state.into()));
    let mut root = Dictionary::new();
    root.insert("settings".into(), Value::Dictionary(settings));
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &Value::Dictionary(root)).unwrap();
    out
}

const SESSION: (&str, &str) = (SESSION_HEADER, "S-1");

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn disabled_hub_ignores_everything() {
    let rig = rig(false, false).await;
    let mut client = Client::connect(rig.addr).await;
    assert_eq!(client.request("PUT", "/photo", &[SESSION], png(4, 3)).await.status, 404);
    assert_eq!(client.request("GET", "/slideshowFeatures", &[], Vec::new()).await.status, 404);
    assert_eq!(*rig.probe.opened.lock().unwrap(), 0);
}

#[tokio::test]
async fn answers_server_info_and_slideshow_features() {
    let rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    let info = client.request("GET", "/server-info", &[], Vec::new()).await;
    let root = Value::from_reader_xml(std::io::Cursor::new(&info.body[..])).unwrap().into_dictionary().unwrap();
    assert_eq!(root.get("features").and_then(Value::as_unsigned_integer), Some(SERVER_INFO_FEATURES));
    assert_eq!(root.get("deviceid").and_then(Value::as_string), Some("AA:BB:CC:DD:EE:FF"));
    let features = client.request("GET", "/slideshowFeatures", &[("Accept-Language", "es")], Vec::new()).await;
    let root = Value::from_reader_xml(std::io::Cursor::new(&features.body[..])).unwrap().into_dictionary().unwrap();
    let themes = root.get("themes").and_then(Value::as_array).unwrap();
    assert_eq!(themes.len(), THEMES.len());
    assert!(themes.iter().any(|t| t.as_dictionary().and_then(|d| d.get("key")).and_then(Value::as_string) == Some("KenBurns")));
}

#[tokio::test]
async fn shows_a_photo_and_stops() {
    let mut rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    let response = client.request("PUT", "/photo", &[SESSION, ("X-Apple-AssetKey", "A"), ("X-Apple-Transition", "Dissolve")], png(6, 4)).await;
    assert_eq!(response.status, 200);
    assert_eq!(*rig.probe.opened.lock().unwrap(), 1);
    assert_eq!(rig.probe.shown.lock().unwrap().as_slice(), &[(6, 4, Transition::Dissolve, None)]);
    assert!(matches!(rig.events.recv().await, Some(RxEvent::PhotosStarted { .. })));
    // A second photo reuses the window; "None" is an instant change.
    let response = client.request("PUT", "/photo", &[SESSION, ("X-Apple-AssetKey", "B"), ("X-Apple-Transition", "None")], png(3, 5)).await;
    assert_eq!(response.status, 200);
    assert_eq!(*rig.probe.opened.lock().unwrap(), 1);
    assert_eq!(rig.probe.shown.lock().unwrap()[1], (3, 5, Transition::None, None));
    assert_eq!(client.request("POST", "/stop", &[SESSION], Vec::new()).await.status, 200);
    assert!(rig.probe.closed.load(Ordering::Acquire));
    assert!(matches!(rig.events.recv().await, Some(RxEvent::PhotosStopped)));
    // Nothing to stop any more: the request is not ours.
    assert_eq!(client.request("POST", "/stop", &[SESSION], Vec::new()).await.status, 404);
}

#[tokio::test]
async fn preloads_with_cache_only_and_displays_cached() {
    let rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    let cached = client
        .request("PUT", "/photo", &[SESSION, ("X-Apple-AssetKey", "K"), ("X-Apple-AssetAction", "cacheOnly")], png(8, 2))
        .await;
    assert_eq!(cached.status, 200);
    // Preloading neither opens the window nor shows anything.
    assert_eq!(*rig.probe.opened.lock().unwrap(), 0);
    let shown = client.request("PUT", "/photo", &[SESSION, ("X-Apple-AssetKey", "K"), ("X-Apple-AssetAction", "displayCached")], Vec::new()).await;
    assert_eq!(shown.status, 200);
    assert_eq!(rig.probe.shown.lock().unwrap().as_slice(), &[(8, 2, Transition::Dissolve, None)]);
    let missing = client.request("PUT", "/photo", &[SESSION, ("X-Apple-AssetKey", "nope"), ("X-Apple-AssetAction", "displayCached")], Vec::new()).await;
    assert_eq!(missing.status, 412);
    let empty = client.request("PUT", "/photo", &[SESSION], Vec::new()).await;
    assert_eq!(empty.status, 400);
}

#[tokio::test]
async fn rejects_pictures_that_cannot_be_decoded() {
    let rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    let response = client.request("PUT", "/photo", &[SESSION], b"this is not an image".to_vec()).await;
    assert_eq!(response.status, 415);
    assert_eq!(*rig.probe.opened.lock().unwrap(), 0);
}

#[tokio::test]
async fn reports_a_window_that_cannot_open() {
    let rig = rig(true, false).await;
    rig.probe.fail_open.store(true, Ordering::Release);
    let mut client = Client::connect(rig.addr).await;
    assert_eq!(client.request("PUT", "/photo", &[SESSION], png(2, 2)).await.status, 500);
}

#[tokio::test]
async fn closing_the_window_ends_the_session() {
    let mut rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    assert_eq!(client.request("PUT", "/photo", &[SESSION], png(2, 2)).await.status, 200);
    assert!(matches!(rig.events.recv().await, Some(RxEvent::PhotosStarted { .. })));
    rig.probe.closed.store(true, Ordering::Release);
    assert!(matches!(tokio::time::timeout(Duration::from_secs(5), rig.events.recv()).await, Ok(Some(RxEvent::PhotosStopped))));
    // The device's next photo closes its connection; a new session is welcome.
    client.send("PUT", "/photo", &[SESSION], png(2, 2)).await;
    let response = client.response().await;
    assert_eq!(response.status, 403);
    assert_eq!(response.headers.get("Connection"), Some("close"));
    let mut other = Client::connect(rig.addr).await;
    assert_eq!(other.request("PUT", "/photo", &[(SESSION_HEADER, "S-2")], png(2, 2)).await.status, 200);
    assert_eq!(*rig.probe.opened.lock().unwrap(), 2);
}

#[tokio::test]
async fn pulls_slideshow_assets_and_advances() {
    let mut rig = rig(true, true).await;
    let mut reverse = Client::connect(rig.addr).await;
    let upgrade = reverse.request("POST", "/reverse", &[SESSION, ("X-Apple-Purpose", "slideshow"), ("Upgrade", "PTTH/1.0")], Vec::new()).await;
    assert_eq!(upgrade.status, 101);
    let mut client = Client::connect(rig.addr).await;
    let put = client.request("PUT", "/slideshows/1", &[SESSION], slideshow_body(1, "KenBurns", "playing")).await;
    assert_eq!(put.status, 200);
    assert!(matches!(rig.events.recv().await, Some(RxEvent::PhotosStarted { .. })));

    let first = reverse.incoming().await;
    assert_eq!((first.method.as_str(), first.uri.as_str()), ("GET", "/slideshows/1/assets/1"));
    reverse.answer(200, "OK", png(5, 3)).await;
    let second = reverse.incoming().await;
    assert_eq!(second.uri, "/slideshows/1/assets/2");
    reverse.answer(200, "OK", png(3, 5)).await;
    let third = reverse.incoming().await;
    assert_eq!(third.uri, "/slideshows/1/assets/3");
    reverse.answer(404, "Not Found", Vec::new()).await;

    // The first picture appears at once, the second after the slide duration (1 s), then it loops.
    eventually("three slides", || rig.probe.shown.lock().unwrap().len() >= 3).await;
    let shown = rig.probe.shown.lock().unwrap().clone();
    assert_eq!((shown[0].0, shown[0].1), (5, 3));
    assert_eq!((shown[1].0, shown[1].1), (3, 5));
    assert_eq!((shown[2].0, shown[2].1), (5, 3));
    // Ken Burns lasts one slide plus the cross-fade.
    assert_eq!(shown[1].3, Some(Duration::from_secs(1) + Duration::from_secs_f32(DISSOLVE_SECONDS)));
    assert_eq!(shown[1].2, Transition::Dissolve);

    // Pausing keeps the picture; deleting the slideshow closes the window.
    let paused = client.request("PUT", "/slideshows/1", &[SESSION], slideshow_body(1, "KenBurns", "paused")).await;
    assert_eq!(paused.status, 200);
    let count = rig.probe.shown.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(rig.probe.shown.lock().unwrap().len() <= count + 1);
    assert_eq!(client.request("DELETE", "/slideshows/1", &[SESSION], Vec::new()).await.status, 200);
    assert!(rig.probe.closed.load(Ordering::Acquire));
}

#[tokio::test]
async fn accepts_pushed_assets_and_bad_slideshows() {
    let rig = rig(true, false).await;
    let mut client = Client::connect(rig.addr).await;
    assert_eq!(client.request("PUT", "/slideshows/1", &[SESSION], b"junk".to_vec()).await.status, 400);
    assert_eq!(client.request("PUT", "/slideshows/1/assets/1", &[SESSION], Vec::new()).await.status, 400);
    assert_eq!(client.request("PUT", "/slideshows/1/assets/1", &[SESSION], png(4, 4)).await.status, 200);
    assert_eq!(client.request("PUT", "/slideshows/1/assets/2", &[SESSION], png(4, 6)).await.status, 200);
    let put = client.request("PUT", "/slideshows/1", &[SESSION], slideshow_body(2, "Classic", "playing")).await;
    assert_eq!(put.status, 200);
    eventually("the first pushed picture", || !rig.probe.shown.lock().unwrap().is_empty()).await;
    assert_eq!(rig.probe.shown.lock().unwrap()[0], (4, 4, Transition::Dissolve, None));
    let stopped = client.request("PUT", "/slideshows/1", &[SESSION], slideshow_body(2, "Classic", "stopped")).await;
    assert_eq!(stopped.status, 200);
    assert!(rig.probe.closed.load(Ordering::Acquire));
    assert!(rig.hub.lock().slideshow.is_none());
}

#[tokio::test]
async fn leaves_the_video_reverse_channel_to_the_video_hub() {
    let rig = rig(true, true).await;
    let mut client = Client::connect(rig.addr).await;
    // With videos on, a normal event channel is not ours (the test server answers 404).
    let event = client.request("POST", "/reverse", &[SESSION, ("X-Apple-Purpose", "event")], Vec::new()).await;
    assert_eq!(event.status, 404);
    let off = self::rig(true, false).await;
    let mut other = Client::connect(off.addr).await;
    let upgrade = other.request("POST", "/reverse", &[SESSION, ("X-Apple-Purpose", "event")], Vec::new()).await;
    assert_eq!(upgrade.status, 101);
}
