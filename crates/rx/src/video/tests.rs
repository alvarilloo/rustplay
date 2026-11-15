// Loopback tests of the AirPlay video handlers: a tiny server drives the Hub with the real HTTP
// codec, and a fake player stands in for the window. Everything binds to 127.0.0.1.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use plist::{Dictionary, Value};
use rustplay_core::media::player::{PlaybackStatus, PlayerCommand, StartAt};
use rustplay_core::rtsp::{ClientCodec, Headers, Protocol, Request, Response, ServerCodec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Default)]
struct Probe {
    opened: Mutex<Vec<(String, StartAt)>>,
    commands: Mutex<Vec<PlayerCommand>>,
    status: Mutex<PlaybackStatus>,
}

struct FakePlayer(Arc<Probe>);

impl Player for FakePlayer {
    fn status(&self) -> PlaybackStatus {
        *self.0.status.lock().unwrap()
    }

    fn command(&self, command: PlayerCommand) {
        self.0.commands.lock().unwrap().push(command);
    }
}

fn factory(probe: &Arc<Probe>) -> PlayerFactory {
    let probe = Arc::clone(probe);
    Arc::new(move |request: PlayerRequest| {
        probe.opened.lock().unwrap().push((request.url, request.start));
        Ok(Box::new(FakePlayer(Arc::clone(&probe))) as Box<dyn Player>)
    })
}

struct Rig {
    hub: Arc<Hub>,
    probe: Arc<Probe>,
    addr: std::net::SocketAddr,
    _events: mpsc::Receiver<RxEvent>,
    cancel: CancellationToken,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn rig(enabled: bool) -> Rig {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let probe = Arc::new(Probe::default());
    let (events_tx, events_rx) = mpsc::channel(64);
    let hub = Hub::new(
        enabled,
        addr.port(),
        "PC".into(),
        "AA:BB:CC:DD:EE:FF".into(),
        vec!["es".into()],
        factory(&probe),
        events_tx,
    );
    let cancel = CancellationToken::new();
    let (server_hub, server_cancel) = (Arc::clone(&hub), cancel.clone());
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { return };
            tokio::spawn(serve(stream, peer, Arc::clone(&server_hub), server_cancel.clone()));
        }
    });
    Rig { hub, probe, addr, _events: events_rx, cancel }
}

// What rustplay-rx's connection loop does with the hub, minus pairing and RTSP.
async fn serve(mut stream: TcpStream, peer: std::net::SocketAddr, hub: Arc<Hub>, cancel: CancellationToken) {
    let mut codec = ServerCodec;
    let mut inbound = BytesMut::new();
    let mut out = BytesMut::new();
    loop {
        while let Ok(Some(request)) = codec.decode(&mut inbound) {
            let (response, reverse) = hub
                .handle(&request, peer)
                .unwrap_or_else(|| (Response::new(request.protocol, 404, "Not Found"), None));
            out.clear();
            codec.encode(response, &mut out).unwrap();
            stream.write_all(&out).await.unwrap();
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

    async fn send(&mut self, method: &str, uri: &str, session: Option<&str>, body: Vec<u8>, content_type: Option<&str>) {
        let mut headers = Headers::default();
        if let Some(session) = session {
            headers.set(SESSION_HEADER, session);
        }
        if let Some(content_type) = content_type {
            headers.set("Content-Type", content_type);
        }
        let request =
            Request { method: method.into(), uri: uri.into(), protocol: Protocol::Http11, headers, body: Bytes::from(body) };
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

    async fn request(&mut self, method: &str, uri: &str, session: Option<&str>) -> Response {
        self.send(method, uri, session, Vec::new(), None).await;
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
}

fn binary(dict: Dictionary) -> Vec<u8> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(dict)).unwrap();
    out
}

fn play_body(location: &str, seconds: f64) -> Vec<u8> {
    let mut d = Dictionary::new();
    d.insert("Content-Location".into(), Value::String(location.into()));
    d.insert("Start-Position-Seconds".into(), Value::Real(seconds));
    d.insert("uuid".into(), Value::String("UUID-1".into()));
    binary(d)
}

fn fcup_answer(url: &str, data: &str) -> Vec<u8> {
    let mut params = Dictionary::new();
    params.insert("FCUP_Response_URL".into(), Value::String(url.into()));
    params.insert("FCUP_Response_Data".into(), Value::Data(data.as_bytes().to_vec()));
    params.insert("FCUP_Response_StatusCode".into(), Value::Integer(200.into()));
    let mut d = Dictionary::new();
    d.insert("type".into(), Value::String("unhandledURLResponse".into()));
    d.insert("params".into(), Value::Dictionary(params));
    binary(d)
}

fn fcup_url(request: &Request) -> String {
    let root = Value::from_reader_xml(std::io::Cursor::new(&request.body[..])).unwrap().into_dictionary().unwrap();
    assert_eq!(root.get("type").and_then(Value::as_string), Some("unhandledURLRequest"));
    root.get("request")
        .and_then(Value::as_dictionary)
        .and_then(|r| r.get("FCUP_Response_URL"))
        .and_then(Value::as_string)
        .unwrap()
        .to_owned()
}

const BINARY: &str = "application/x-apple-binary-plist";

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..100 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met in time");
}

#[tokio::test]
async fn disabled_hub_ignores_every_video_request() {
    let rig = rig(false).await;
    let mut client = Client::connect(rig.addr).await;
    for (method, uri) in [("POST", "/play"), ("GET", "/playback-info"), ("POST", "/stop"), ("GET", "/server-info")] {
        assert_eq!(client.request(method, uri, Some("S")).await.status, 404, "{method} {uri}");
    }
    assert!(!rig.hub.enabled());
}

#[tokio::test]
async fn plays_scrubs_pauses_and_stops_a_plain_url() {
    let rig = rig(true).await;
    let mut client = Client::connect(rig.addr).await;
    client.send("POST", "/play", Some("S1"), play_body("https://example.com/v.mp4", 12.5), Some(BINARY)).await;
    assert_eq!(client.response().await.status, 200);
    assert_eq!(*rig.probe.opened.lock().unwrap(), [("https://example.com/v.mp4".to_owned(), StartAt::Seconds(12.5))]);

    // Not open yet: an empty answer.
    let info = client.request("GET", "/playback-info", Some("S1")).await;
    assert_eq!((info.status, info.body.len()), (200, 0));

    *rig.probe.status.lock().unwrap() =
        PlaybackStatus { opened: true, playing: true, duration: 100.0, position: 12.5, ..Default::default() };
    let info = client.request("GET", "/playback-info", Some("S1")).await;
    let dict = Value::from_reader_xml(std::io::Cursor::new(&info.body[..])).unwrap().into_dictionary().unwrap();
    assert_eq!(dict.get("duration").and_then(Value::as_real), Some(100.0));
    assert_eq!(dict.get("position").and_then(Value::as_real), Some(12.5));
    assert_eq!(dict.get("rate").and_then(Value::as_real), Some(1.0));
    assert_eq!(dict.get("readyToPlay").and_then(Value::as_unsigned_integer), Some(1));
    let seekable = dict.get("seekableTimeRanges").and_then(Value::as_array).unwrap();
    assert_eq!(seekable[0].as_dictionary().and_then(|d| d.get("duration")).and_then(Value::as_real), Some(100.0));

    assert_eq!(client.request("POST", "/scrub?position=40.25", Some("S1")).await.status, 200);
    assert_eq!(client.request("POST", "/rate?value=0.000000", Some("S1")).await.status, 200);
    assert_eq!(client.request("POST", "/rate?value=1.000000", Some("S1")).await.status, 200);
    let scrub = client.request("GET", "/scrub", Some("S1")).await;
    assert!(String::from_utf8_lossy(&scrub.body).contains("duration: 100.000000"));
    assert_eq!(
        *rig.probe.commands.lock().unwrap(),
        // The first command is the device's volume (full by default) given to the new player.
        [PlayerCommand::Volume(1.0), PlayerCommand::Seek(40.25), PlayerCommand::Pause, PlayerCommand::Play]
    );
    // The device's volume buttons reach the video being played, and the bottom of the slider mutes it.
    rig.hub.set_volume(crate::proto::db_to_linear(-30.0));
    rig.hub.set_volume(crate::proto::db_to_linear(0.0));
    assert_eq!(
        rig.probe.commands.lock().unwrap()[4..],
        [PlayerCommand::Volume(0.0), PlayerCommand::Volume(1.0)]
    );

    assert_eq!(client.request("POST", "/stop", Some("S1")).await.status, 200);
    assert_eq!(rig.probe.commands.lock().unwrap().last(), Some(&PlayerCommand::Stop));
    let info = client.request("GET", "/playback-info", Some("S1")).await;
    assert_eq!(info.body.len(), 0);
}

#[tokio::test]
async fn rejects_malformed_play_requests() {
    let rig = rig(true).await;
    let mut client = Client::connect(rig.addr).await;
    client.send("POST", "/play", Some("S"), b"bplist00garbage".to_vec(), Some(BINARY)).await;
    assert_eq!(client.response().await.status, 400);
    client.send("POST", "/play", None, play_body("http://x/y.mp4", 0.0), Some(BINARY)).await;
    assert_eq!(client.response().await.status, 400);
    client.send("POST", "/play", Some("S"), play_body("ftp://x/y", 0.0), Some(BINARY)).await;
    assert_eq!(client.response().await.status, 400);
    assert!(rig.probe.opened.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ends_the_video_when_the_window_closes() {
    let rig = rig(true).await;
    let mut client = Client::connect(rig.addr).await;
    client.send("POST", "/play", Some("S1"), play_body("http://x/v.mp4", 0.0), Some(BINARY)).await;
    assert_eq!(client.response().await.status, 200);
    *rig.probe.status.lock().unwrap() = PlaybackStatus { opened: true, closed: true, ..Default::default() };
    let hub = Arc::clone(&rig.hub);
    wait_until(move || hub.lock().current.is_none()).await;
    // The client's next poll gets a closing answer so it leaves its AirPlay mode.
    let info = client.request("GET", "/playback-info", Some("S1")).await;
    assert_eq!(info.headers.get("Connection"), Some("close"));
}

#[tokio::test]
async fn gets_hls_playlists_through_the_reverse_channel() {
    let rig = rig(true).await;
    let mut main = Client::connect(rig.addr).await;
    let mut reverse = Client::connect(rig.addr).await;
    let upgrade = reverse.request("POST", "/reverse", Some("YT")).await;
    assert_eq!(upgrade.status, 101);
    assert_eq!(upgrade.headers.get("Upgrade"), Some("PTTH/1.0"));

    main.send("POST", "/play", Some("YT"), play_body("mlhls://localhost/master.m3u8", 0.0), Some(BINARY)).await;
    assert_eq!(main.response().await.status, 200);
    assert!(rig.probe.opened.lock().unwrap().is_empty(), "the player waits for the playlists");

    let first = reverse.incoming().await;
    assert_eq!((first.method.as_str(), first.uri.as_str()), ("POST", "/event"));
    assert_eq!(fcup_url(&first), "mlhls://localhost/master.m3u8");
    let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720\nmlhls://localhost/itag/136.m3u8\n";
    main.send("POST", "/action", Some("YT"), fcup_answer("mlhls://localhost/master.m3u8", master), Some(BINARY)).await;
    assert_eq!(main.response().await.status, 200);

    let second = reverse.incoming().await;
    assert_eq!(fcup_url(&second), "mlhls://localhost/itag/136.m3u8");
    let media = "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5.0,\nhttps://r1.example/seg0.ts\n#EXT-X-ENDLIST\n";
    main.send("POST", "/action", Some("YT"), fcup_answer("mlhls://localhost/itag/136.m3u8", media), Some(BINARY)).await;
    assert_eq!(main.response().await.status, 200);

    let url = format!("http://127.0.0.1:{}/master.m3u8", rig.addr.port());
    let probe = Arc::clone(&rig.probe);
    wait_until(move || !probe.opened.lock().unwrap().is_empty()).await;
    assert_eq!(rig.probe.opened.lock().unwrap()[0].0, url);

    // The local player fetches the playlists from this same port, with our address in them.
    let mut player = Client::connect(rig.addr).await;
    let served = player.request("GET", "/master.m3u8", None).await;
    assert_eq!(served.status, 200);
    let text = String::from_utf8(served.body.to_vec()).unwrap();
    assert!(text.contains(&format!("http://127.0.0.1:{}/itag/136.m3u8", rig.addr.port())));
    assert!(!text.contains("mlhls"));
    let served = player.request("GET", "/itag/136.m3u8", None).await;
    assert_eq!(served.status, 200);
    assert!(String::from_utf8_lossy(&served.body).contains("https://r1.example/seg0.ts"));
    assert_eq!(player.request("GET", "/itag/999.m3u8", None).await.status, 404);

    // A client request with a session header is never taken for the local player.
    assert_eq!(player.request("GET", "/master.m3u8", Some("YT")).await.status, 404);
}

#[tokio::test]
async fn pending_fcup_request_is_sent_when_the_reverse_channel_opens_late() {
    let rig = rig(true).await;
    let mut main = Client::connect(rig.addr).await;
    main.send("POST", "/play", Some("YT"), play_body("mlhls://localhost/master.m3u8", 0.0), Some(BINARY)).await;
    assert_eq!(main.response().await.status, 200);
    let mut reverse = Client::connect(rig.addr).await;
    assert_eq!(reverse.request("POST", "/reverse", Some("YT")).await.status, 101);
    assert_eq!(fcup_url(&reverse.incoming().await), "mlhls://localhost/master.m3u8");
}

#[tokio::test]
async fn server_info_lists_video_features() {
    let rig = rig(true).await;
    let mut client = Client::connect(rig.addr).await;
    let response = client.request("GET", "/server-info", None).await;
    let dict = Value::from_reader_xml(std::io::Cursor::new(&response.body[..])).unwrap().into_dictionary().unwrap();
    assert_eq!(dict.get("features").and_then(Value::as_unsigned_integer), Some(SERVER_INFO_FEATURES));
    assert_eq!(dict.get("deviceid").and_then(Value::as_string), Some("AA:BB:CC:DD:EE:FF"));
}

#[test]
fn recognizes_http_locations() {
    assert!(is_http("https://a/b.mp4"));
    assert!(is_http("HTTP://a"));
    assert!(!is_http("mlhls://localhost/master.m3u8"));
    let _ = VecDeque::<u8>::new();
}
