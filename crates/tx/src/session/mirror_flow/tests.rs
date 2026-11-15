use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use rustplay_core::identity::Identity;
use rustplay_core::rtsp::ServerCodec;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::codec::{Decoder, Encoder};

use super::*;
use crate::info::fixtures;

type Entry = (String, String, Vec<(String, String)>, Vec<u8>);

fn bplist(entries: Vec<(&str, Value)>) -> Vec<u8> {
    let mut d = Dictionary::new();
    for (k, v) in entries {
        d.insert(k.to_owned(), v);
    }
    setup::to_binary(d).unwrap()
}

fn uint(v: u64) -> Value {
    Value::Integer(v.into())
}

fn stream(kind: u64, entries: &[(&str, u64)]) -> Value {
    let mut d = Dictionary::new();
    d.insert("type".into(), uint(kind));
    for (k, v) in entries {
        d.insert((*k).to_owned(), uint(*v));
    }
    Value::Array(vec![Value::Dictionary(d)])
}

async fn fake_receiver(log: Arc<StdMutex<Vec<Entry>>>, data_port: u16) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut codec = ServerCodec;
        let mut inbound = BytesMut::new();
        let mut setups = 0;
        loop {
            while let Some(request) = codec.decode(&mut inbound).unwrap() {
                let headers = request.headers.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
                log.lock()
                    .unwrap()
                    .push((request.method.clone(), request.uri.clone(), headers, request.body.to_vec()));
                let mut response = rustplay_core::rtsp::Response::ok(request.protocol);
                response.headers.set("CSeq", request.cseq().unwrap_or("0"));
                if request.method == "SETUP" {
                    setups += 1;
                    let body = match setups {
                        1 => {
                            let mut display = Dictionary::new();
                            display.insert("widthPixels".into(), uint(1920));
                            display.insert("heightPixels".into(), uint(1080));
                            let mut info = Dictionary::new();
                            info.insert("displays".into(), Value::Array(vec![Value::Dictionary(display)]));
                            bplist(vec![("info", Value::Dictionary(info))])
                        }
                        2 => bplist(vec![("streams", stream(96, &[("dataPort", 7001), ("controlPort", 7002)]))]),
                        _ => bplist(vec![("streams", stream(110, &[("dataPort", u64::from(data_port))]))]),
                    };
                    response = response.with_body("application/x-apple-binary-plist", body.into());
                }
                let mut out = BytesMut::new();
                codec.encode(response, &mut out).unwrap();
                socket.write_all(&out).await.unwrap();
            }
            if socket.read_buf(&mut inbound).await.unwrap() == 0 {
                return;
            }
        }
    });
    addr
}

fn inert_spawn(
    _: VideoParams,
    shared: Arc<Shared>,
    _: mpsc::Receiver<Vec<u8>>,
    _: mpsc::Sender<crate::video::Unit>,
) -> Result<VideoThread> {
    Ok(VideoThread::inert(shared))
}

fn plist_of(body: &[u8]) -> Dictionary {
    Value::from_reader(std::io::Cursor::new(body)).unwrap().into_dictionary().unwrap()
}

#[tokio::test]
async fn mirroring_flow_follows_doubletake_request_order() {
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_port = data_listener.local_addr().unwrap().port();
    let log: Arc<StdMutex<Vec<Entry>>> = Arc::default();
    let addr = fake_receiver(Arc::clone(&log), data_port).await;
    let dir = std::env::temp_dir().join(format!("rustplay-tx-test-{}", rand::random::<u32>()));
    let mut info = fixtures::apple_tv();
    info.features |= 1 << 7 | 1 << 28;
    let mut session = Session {
        conn: Arc::new(Mutex::new(RtspConn::connect(addr).await.unwrap())),
        info,
        identity: Identity::load_or_create_in(&dir).unwrap(),
        peer: addr,
        name: "PC".into(),
        encrypted: true,
        secret: Some(vec![5u8; 32]),
        fairplay: None,
        session_uuid: "X".into(),
        audio_uri: None,
    };
    let options = MirrorOptions { monitor: 0, fps: 30, bitrate: 8_000_000, audio: false, ..MirrorOptions::default() };
    let handle = start_with(&mut session, options, inert_spawn).await.unwrap();
    assert_eq!(handle.video_size(), Some((1920, 1080)));
    let (accepted, _) = data_listener.accept().await.unwrap();
    drop(accepted);
    handle.stop().await.unwrap();

    let log = log.lock().unwrap();
    let methods: Vec<&str> = log.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(&methods[..4], ["SETUP", "SETUP", "SETUP", "RECORD"]);
    assert!(methods.contains(&"TEARDOWN"));
    assert!(!methods.contains(&"SET_PARAMETER"), "no volume without audio");

    let control = plist_of(&log[0].3);
    assert_eq!(control["isScreenMirroringSession"].as_boolean(), Some(true));
    assert_eq!(control["updateSessionRequest"].as_boolean(), Some(false));
    assert!(!control.contains_key("streams"));
    let audio = plist_of(&log[1].3);
    let audio_stream = audio["streams"].as_array().unwrap()[0].as_dictionary().unwrap();
    assert_eq!(audio_stream["type"].as_unsigned_integer(), Some(96));
    assert_eq!(audio_stream["usingScreen"].as_boolean(), Some(true));
    let latency = audio_latency_samples(std::env::var("RUSTPLAY_TX_AUDIO_LATENCY_MS").ok().as_deref());
    assert_eq!(audio_stream["latencyMin"].as_unsigned_integer(), Some(0));
    assert_eq!(audio_stream["latencyMax"].as_unsigned_integer(), Some(u64::from(latency)));
    assert!(audio_stream.contains_key("streamConnections") && !audio.contains_key("sessionUUID"));
    let video = plist_of(&log[2].3);
    let video_stream = video["streams"].as_array().unwrap()[0].as_dictionary().unwrap();
    assert_eq!(video_stream["type"].as_unsigned_integer(), Some(110));
    assert_eq!(video_stream["latencyMs"].as_unsigned_integer(), Some(75));
    assert_eq!(video_stream["shk"].as_data().map(<[u8]>::len), Some(16));
    assert_eq!(video_stream["shiv"].as_data().map(<[u8]>::len), Some(16));
    assert_ne!(log[0].1, log[2].1, "the video stream has its own URI");
    assert_eq!(log[0].1, log[1].1, "control and audio share a URI");
    assert_eq!(log[3].1, log[0].1);
    let headers: HashMap<_, _> = log[3].2.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
    assert_eq!(headers["range"], "npt=0-");
    assert_eq!(headers["rtp-info"], "seq=0;rtptime=0");
    assert_eq!(headers["session"], control["sessionUUID"].as_string().unwrap());
    assert!(!headers.contains_key("dacp-id") && !headers.contains_key("client-instance"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn screen_audio_latency_defaults_to_the_ios_lead_and_can_be_overridden() {
    assert_eq!(audio_latency_samples(None), 7497);
    assert_eq!(AUDIO_LATENCY_SAMPLES * 1000 / 44_100, 170);
    assert_eq!(audio_latency_samples(Some("250")), 11_025);
    assert_eq!(audio_latency_samples(Some(" 85 ")), 3748);
    assert_eq!(audio_latency_samples(Some("1")), 2205, "below 50 ms is clamped");
    assert_eq!(audio_latency_samples(Some("60000")), 88_200, "above 2 s is clamped");
    assert_eq!(audio_latency_samples(Some("rápido")), 7497);
}

#[test]
fn canvas_height_cap_keeps_the_aspect_ratio() {
    assert_eq!(super::cap_height((1920, 1080), Some(720)), (1280, 720));
    assert_eq!(super::cap_height((3840, 2160), Some(1080)), (1920, 1080));
    assert_eq!(super::cap_height((1280, 720), Some(1080)), (1280, 720));
    assert_eq!(super::cap_height((1920, 1080), None), (1920, 1080));
}
