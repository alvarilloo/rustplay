use rustplay_core::{Error, Result};
use plist::{Dictionary, Value};

use crate::setup::{ALAC_AUDIO_FORMAT, ALAC_COMPRESSION_TYPE, Layout};

pub const STREAM_TYPE_SCREEN_AUDIO: u64 = 96;
pub const STREAM_TYPE_VIDEO: u64 = 110;
pub const REDUNDANT_AUDIO: u64 = 2;
pub const SAMPLE_RATE: u64 = 44_100;
pub const SPF: u64 = 352;

pub const TIMESTAMP_INFO: [&str; 5] = ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"];

fn uint(value: u64) -> Value {
    Value::Integer(value.into())
}

fn text(value: &str) -> Value {
    Value::String(value.into())
}

pub struct MirrorSetup<'a> {
    pub device_id: &'a str,
    pub session_uuid: &'a str,
    pub source_version: &'a str,
    pub timing_port: u16,
    pub name: &'a str,
}

pub type FairPlayRoot<'a> = Option<(&'a [u8], &'a [u8])>;

// Session-level keys of a mirroring SETUP (NTP timing only: 377.40.x receivers do not take PTP).
pub fn session_plist(setup: &MirrorSetup) -> Dictionary {
    let mut d = Dictionary::new();
    d.insert("deviceID".into(), text(setup.device_id));
    d.insert("macAddress".into(), text(setup.device_id));
    d.insert("sessionUUID".into(), text(setup.session_uuid));
    d.insert("sourceVersion".into(), text(setup.source_version));
    d.insert("isScreenMirroringSession".into(), Value::Boolean(true));
    d.insert("timingProtocol".into(), text("NTP"));
    d.insert("osBuildVersion".into(), text("13F69"));
    d.insert("model".into(), text("Linux"));
    d.insert("name".into(), text(setup.name));
    d.insert("timingPort".into(), uint(u64::from(setup.timing_port)));
    d
}

fn add_fairplay_root(d: &mut Dictionary, root: FairPlayRoot) {
    if let Some((ekey, eiv)) = root.filter(|(k, i)| !k.is_empty() && !i.is_empty()) {
        d.insert("ekey".into(), Value::Data(ekey.to_vec()));
        d.insert("eiv".into(), Value::Data(eiv.to_vec()));
        d.insert("et".into(), uint(32));
    }
}

// The control SETUP carries no streams; combined GET /info makes the receiver return its display
// metadata, which does not exist before the media session does.
pub fn control_plist(setup: &MirrorSetup, root: FairPlayRoot) -> Dictionary {
    let mut d = session_plist(setup);
    d.insert("updateSessionRequest".into(), Value::Boolean(false));
    d.insert("combinedGetInfoWithControlSetup".into(), Value::Boolean(true));
    add_fairplay_root(&mut d, root);
    d
}

fn streams(stream: Dictionary) -> Dictionary {
    let mut d = Dictionary::new();
    d.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
    d
}

pub fn streams_only(stream: Dictionary) -> Dictionary {
    streams(stream)
}

// Media-first order (the receiver rejected the control SETUP): the first stream creates the session.
pub fn streams_with_session(setup: &MirrorSetup, stream: Dictionary, root: FairPlayRoot) -> Dictionary {
    let mut d = session_plist(setup);
    d.extend(streams(stream));
    add_fairplay_root(&mut d, root);
    d
}

pub fn screen_audio_stream(
    stream_connection_id: u64,
    latency_samples: u32,
    shk: Option<&[u8; 32]>,
    control_port: u16,
    layout: Layout,
) -> Dictionary {
    let mut s = Dictionary::new();
    s.insert("type".into(), uint(STREAM_TYPE_SCREEN_AUDIO));
    s.insert("streamConnectionID".into(), uint(stream_connection_id));
    let (ct, format) = if crate::audio::pcm_mode() { (1, 0x800) } else { (ALAC_COMPRESSION_TYPE, ALAC_AUDIO_FORMAT) };
    s.insert("ct".into(), uint(ct));
    s.insert("spf".into(), uint(SPF));
    s.insert("sr".into(), uint(SAMPLE_RATE));
    s.insert("audioFormat".into(), uint(format));
    s.insert("audioMode".into(), text("default"));
    s.insert("usingScreen".into(), Value::Boolean(true));
    s.insert("latencyMin".into(), uint(0));
    s.insert("latencyMax".into(), uint(u64::from(latency_samples)));
    s.insert("redundantAudio".into(), uint(REDUNDANT_AUDIO));
    match layout {
        Layout::ControlPort => {
            s.insert("controlPort".into(), uint(u64::from(control_port)));
            if let Some(key) = shk {
                s.insert("shk".into(), Value::Data(key.to_vec()));
            }
        }
        Layout::StreamConnections => {
            if let Some(key) = shk {
                s.insert("shk".into(), Value::Data(key.to_vec()));
            }
            s.insert("isMedia".into(), Value::Boolean(false));
            s.insert("supportsDynamicStreamID".into(), Value::Boolean(true));
            let mut rtp = Dictionary::new();
            rtp.insert("streamConnectionKeyUseStreamEncryptionKey".into(), Value::Boolean(shk.is_some()));
            let mut rtcp = Dictionary::new();
            rtcp.insert("streamConnectionKeyPort".into(), uint(u64::from(control_port)));
            let mut connections = Dictionary::new();
            connections.insert("streamConnectionTypeRTP".into(), Value::Dictionary(rtp));
            connections.insert("streamConnectionTypeRTCP".into(), Value::Dictionary(rtcp));
            s.insert("streamConnections".into(), Value::Dictionary(connections));
        }
    }
    s
}

pub fn video_stream(stream_connection_id: u64, latency_ms: u64, key: Option<(&[u8], &[u8])>) -> Dictionary {
    let mut s = Dictionary::new();
    s.insert("type".into(), uint(STREAM_TYPE_VIDEO));
    s.insert("streamConnectionID".into(), uint(stream_connection_id));
    s.insert("latencyMs".into(), uint(latency_ms));
    let info = TIMESTAMP_INFO
        .iter()
        .map(|name| {
            let mut entry = Dictionary::new();
            entry.insert("name".into(), text(name));
            Value::Dictionary(entry)
        })
        .collect();
    s.insert("timestampInfo".into(), Value::Array(info));
    if let Some((shk, shiv)) = key {
        s.insert("shk".into(), Value::Data(shk.to_vec()));
        s.insert("shiv".into(), Value::Data(shiv.to_vec()));
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamPorts {
    pub data: u16,
    pub control: u16,
}

#[derive(Debug, Clone, Default)]
pub struct MirrorReply {
    pub timing_port: u16,
    pub event_port: u16,
    pub skip_record: bool,
    pub info: Option<Dictionary>,
    pub audio: Option<StreamPorts>,
    pub video_data_port: u16,
}

fn port(value: Option<&Value>) -> u16 {
    value
        .and_then(Value::as_unsigned_integer)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0)
}

// Legacy dataPort/controlPort, overridden by streamConnections ports when present.
pub fn stream_ports(stream: &Dictionary) -> StreamPorts {
    let mut ports = StreamPorts { data: port(stream.get("dataPort")), control: port(stream.get("controlPort")) };
    if let Some(connections) = stream.get("streamConnections").and_then(Value::as_dictionary) {
        let key = |kind: &str| {
            connections
                .get(kind)
                .and_then(Value::as_dictionary)
                .map(|d| port(d.get("streamConnectionKeyPort")))
                .filter(|&p| p > 0)
        };
        ports.data = key("streamConnectionTypeRTP").unwrap_or(ports.data);
        ports.control = key("streamConnectionTypeRTCP").unwrap_or(ports.control);
    }
    ports
}

pub fn parse_reply(body: &[u8]) -> Result<MirrorReply> {
    if body.is_empty() {
        return Ok(MirrorReply::default());
    }
    let value = Value::from_reader(std::io::Cursor::new(body))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| Error::protocol("la respuesta de SETUP no es un diccionario"))?;
    let mut reply = MirrorReply {
        timing_port: port(dict.get("timingPort")),
        event_port: port(dict.get("eventPort")),
        skip_record: dict.get("skipRecord").and_then(Value::as_boolean).unwrap_or(false),
        info: dict.get("info").and_then(Value::as_dictionary).cloned(),
        ..MirrorReply::default()
    };
    for stream in dict.get("streams").and_then(Value::as_array).into_iter().flatten() {
        let Some(stream) = stream.as_dictionary() else { continue };
        match stream.get("type").and_then(Value::as_unsigned_integer) {
            Some(STREAM_TYPE_SCREEN_AUDIO) => reply.audio = Some(stream_ports(stream)),
            Some(STREAM_TYPE_VIDEO) => reply.video_data_port = port(stream.get("dataPort")),
            _ => {}
        }
    }
    Ok(reply)
}

// RTSP statuses that mean "this request shape is not accepted" (descriptor alternates).
pub fn shape_rejected(status: u16) -> bool {
    matches!(status, 400 | 406 | 415 | 455 | 500 | 501)
}

// Narrower: evidence for a single switch to media-first ordering.
pub fn order_rejected(status: u16) -> bool {
    matches!(status, 400 | 405 | 406 | 415 | 455 | 501)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> MirrorSetup<'static> {
        MirrorSetup {
            device_id: "02:11:22:33:44:55",
            session_uuid: "session",
            source_version: "980.71.1",
            timing_port: 6000,
            name: "sender",
        }
    }

    fn sorted_keys(d: &Dictionary) -> Vec<&str> {
        let mut keys: Vec<&str> = d.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn session_plist_uses_ntp_timing_fields() {
        let d = session_plist(&setup());
        assert_eq!(d["timingProtocol"].as_string(), Some("NTP"));
        assert_eq!(d["timingPort"].as_unsigned_integer(), Some(6000));
        assert_eq!(d["sourceVersion"].as_string(), Some("980.71.1"));
        assert_eq!(d["isScreenMirroringSession"].as_boolean(), Some(true));
        assert_eq!(d["deviceID"].as_string(), d["macAddress"].as_string());
        assert!(!d.contains_key("timingPeerInfo") && !d.contains_key("timingPeerList"));
        assert!(!d.contains_key("streams") && !d.contains_key("ekey"));
    }

    #[test]
    fn control_plist_has_no_streams_and_asks_for_combined_info() {
        let d = control_plist(&setup(), None);
        assert_eq!(d["updateSessionRequest"].as_boolean(), Some(false));
        assert_eq!(d["combinedGetInfoWithControlSetup"].as_boolean(), Some(true));
        assert!(!d.contains_key("streams"));
        assert_eq!(d["sessionUUID"].as_string(), Some("session"));
    }

    #[test]
    fn fairplay_root_fields_are_added_only_when_given() {
        let ekey = [1u8; 72];
        let eiv = [2u8; 16];
        let d = control_plist(&setup(), Some((&ekey, &eiv)));
        assert_eq!(d["ekey"].as_data().map(<[u8]>::len), Some(72));
        assert_eq!(d["eiv"].as_data().map(<[u8]>::len), Some(16));
        assert_eq!(d["et"].as_unsigned_integer(), Some(32));
        assert!(!control_plist(&setup(), None).contains_key("ekey"));
    }

    #[test]
    fn stream_only_requests_repeat_no_session_fields() {
        let key = [3u8; 32];
        let d = streams_only(screen_audio_stream(5, 3748, Some(&key), 6001, Layout::ControlPort));
        assert_eq!(sorted_keys(&d), ["streams"]);
        let media_first = streams_with_session(&setup(), screen_audio_stream(5, 3748, None, 6001, Layout::ControlPort), None);
        assert!(media_first.contains_key("streams") && media_first.contains_key("timingPort"));
    }

    #[test]
    fn screen_audio_descriptor_with_control_port() {
        let key = [3u8; 32];
        let s = screen_audio_stream(77, 3748, Some(&key), 6001, Layout::ControlPort);
        assert_eq!(
            sorted_keys(&s),
            [
                "audioFormat", "audioMode", "controlPort", "ct", "latencyMax", "latencyMin", "redundantAudio", "shk", "spf",
                "sr", "streamConnectionID", "type", "usingScreen"
            ]
        );
        assert_eq!(s["type"].as_unsigned_integer(), Some(96));
        assert_eq!(s["ct"].as_unsigned_integer(), Some(2));
        assert_eq!(s["spf"].as_unsigned_integer(), Some(352));
        assert_eq!(s["sr"].as_unsigned_integer(), Some(44_100));
        assert_eq!(s["audioFormat"].as_unsigned_integer(), Some(0x40000));
        assert_eq!(s["usingScreen"].as_boolean(), Some(true));
        assert_eq!(s["latencyMin"].as_unsigned_integer(), Some(0));
        assert_eq!(s["latencyMax"].as_unsigned_integer(), Some(3748));
        assert_eq!(s["redundantAudio"].as_unsigned_integer(), Some(2));
        assert_eq!(s["controlPort"].as_unsigned_integer(), Some(6001));
        assert_eq!(s["shk"].as_data().map(<[u8]>::len), Some(32));
        assert!(!s.contains_key("isMedia"));
    }

    #[test]
    fn screen_audio_descriptor_with_stream_connections() {
        let key = [3u8; 32];
        let s = screen_audio_stream(1, 1, Some(&key), 6002, Layout::StreamConnections);
        assert!(!s.contains_key("controlPort"));
        assert_eq!(s["isMedia"].as_boolean(), Some(false));
        assert_eq!(s["supportsDynamicStreamID"].as_boolean(), Some(true));
        let connections = s["streamConnections"].as_dictionary().unwrap();
        let rtcp = connections["streamConnectionTypeRTCP"].as_dictionary().unwrap();
        assert_eq!(rtcp["streamConnectionKeyPort"].as_unsigned_integer(), Some(6002));
        let rtp = connections["streamConnectionTypeRTP"].as_dictionary().unwrap();
        assert_eq!(rtp["streamConnectionKeyUseStreamEncryptionKey"].as_boolean(), Some(true));
        let keyless = screen_audio_stream(1, 1, None, 6002, Layout::StreamConnections);
        assert!(!keyless.contains_key("shk"));
    }

    #[test]
    fn video_descriptor_matches_doubletake() {
        let shk = [1u8; 16];
        let shiv = [2u8; 16];
        let s = video_stream(99, 75, Some((&shk, &shiv)));
        assert_eq!(sorted_keys(&s), ["latencyMs", "shiv", "shk", "streamConnectionID", "timestampInfo", "type"]);
        assert_eq!(s["type"].as_unsigned_integer(), Some(110));
        assert_eq!(s["latencyMs"].as_unsigned_integer(), Some(75));
        let names: Vec<&str> = s["timestampInfo"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_dictionary().unwrap()["name"].as_string().unwrap())
            .collect();
        assert_eq!(names, ["SubSu", "BePxT", "AfPxT", "BefEn", "EmEnc"]);
        assert_eq!(s["shk"].as_data(), Some(&shk[..]));
        assert!(!video_stream(1, 1, None).contains_key("shk"));
    }

    #[test]
    fn stream_ports_prefer_stream_connections() {
        let mut legacy = Dictionary::new();
        legacy.insert("dataPort".into(), uint(6100));
        legacy.insert("controlPort".into(), uint(6101));
        assert_eq!(stream_ports(&legacy), StreamPorts { data: 6100, control: 6101 });
        let mut rtp = Dictionary::new();
        rtp.insert("streamConnectionKeyPort".into(), uint(7100));
        let mut rtcp = Dictionary::new();
        rtcp.insert("streamConnectionKeyPort".into(), uint(7101));
        let mut connections = Dictionary::new();
        connections.insert("streamConnectionTypeRTP".into(), Value::Dictionary(rtp));
        connections.insert("streamConnectionTypeRTCP".into(), Value::Dictionary(rtcp));
        legacy.insert("streamConnections".into(), Value::Dictionary(connections));
        assert_eq!(stream_ports(&legacy), StreamPorts { data: 7100, control: 7101 });
    }

    #[test]
    fn reply_parsing_picks_streams_by_type() {
        let mut audio = Dictionary::new();
        audio.insert("type".into(), uint(96));
        audio.insert("dataPort".into(), uint(7001));
        audio.insert("controlPort".into(), uint(7002));
        let mut video = Dictionary::new();
        video.insert("type".into(), uint(110));
        video.insert("dataPort".into(), uint(7003));
        let mut info = Dictionary::new();
        info.insert("name".into(), text("TV"));
        let mut root = Dictionary::new();
        root.insert("timingPort".into(), uint(7004));
        root.insert("eventPort".into(), uint(7005));
        root.insert("skipRecord".into(), Value::Boolean(true));
        root.insert("info".into(), Value::Dictionary(info));
        root.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(audio), Value::Dictionary(video)]),
        );
        let reply = parse_reply(&crate::setup::to_binary(root).unwrap()).unwrap();
        assert_eq!((reply.timing_port, reply.event_port, reply.video_data_port), (7004, 7005, 7003));
        assert!(reply.skip_record);
        assert_eq!(reply.audio, Some(StreamPorts { data: 7001, control: 7002 }));
        assert_eq!(reply.info.unwrap()["name"].as_string(), Some("TV"));
        assert!(parse_reply(&[]).unwrap().audio.is_none());
    }

    #[test]
    fn status_classification_matches_doubletake() {
        for status in [400, 406, 415, 455, 500, 501] {
            assert!(shape_rejected(status), "{status}");
        }
        for status in [400, 405, 406, 415, 455, 501] {
            assert!(order_rejected(status), "{status}");
        }
        assert!(!shape_rejected(401) && !order_rejected(500) && !order_rejected(401));
    }
}
