use rustplay_core::{Error, Result};
use plist::{Dictionary, Value};

pub const ALAC_AUDIO_FORMAT: u64 = 0x40000;
pub const ALAC_COMPRESSION_TYPE: u64 = 2;
pub const STREAM_TYPE_REALTIME_AUDIO: u64 = 96;
pub const LEGACY_SOURCE_VERSION: &str = "280.33";
pub const MODERN_SOURCE_VERSION: &str = "980.71.1";

fn uint(value: u64) -> Value {
    Value::Integer(value.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    ControlPort,
    StreamConnections,
}

impl Layout {
    pub fn other(self) -> Self {
        match self {
            Self::ControlPort => Self::StreamConnections,
            Self::StreamConnections => Self::ControlPort,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    // Audio-only AirPlay 2 sender behaviour modelled on OwnTone.
    #[default]
    OwnTone,
    // Screen-mirroring sender behaviour modelled on doubletake.
    Doubletake,
}

impl Profile {
    pub fn parse(text: &str) -> Self {
        if text.trim().eq_ignore_ascii_case("doubletake") {
            Self::Doubletake
        } else {
            Self::OwnTone
        }
    }

    pub fn from_env() -> Self {
        std::env::var("RUSTPLAY_TX_PROFILE").map_or(Self::OwnTone, |v| Self::parse(&v))
    }
}

pub const OWNTONE_LATENCY_MIN: u64 = 11_025;
pub const OWNTONE_LATENCY_MAX: u64 = 88_200;

pub struct SessionSetup<'a> {
    pub device_id: &'a str,
    pub session_uuid: &'a str,
    pub source_version: &'a str,
    pub timing_port: u16,
    pub name: &'a str,
    pub fairplay: Option<(&'a [u8; 72], &'a [u8; 16])>,
}

pub fn session_plist(setup: &SessionSetup, profile: Profile) -> Dictionary {
    let mut d = Dictionary::new();
    d.insert("deviceID".into(), Value::String(setup.device_id.into()));
    d.insert("sessionUUID".into(), Value::String(setup.session_uuid.into()));
    d.insert("timingPort".into(), uint(u64::from(setup.timing_port)));
    d.insert("timingProtocol".into(), Value::String("NTP".into()));
    d.insert("isMultiSelectAirPlay".into(), Value::Boolean(true));
    d.insert("senderSupportsRelay".into(), Value::Boolean(profile == Profile::OwnTone));
    if profile == Profile::Doubletake {
        d.insert("macAddress".into(), Value::String(setup.device_id.into()));
        d.insert("sourceVersion".into(), Value::String(setup.source_version.into()));
        d.insert("name".into(), Value::String(setup.name.into()));
        d.insert("model".into(), Value::String("Windows".into()));
        d.insert("osName".into(), Value::String("Windows".into()));
        d.insert("osBuildVersion".into(), Value::String("13F69".into()));
        d.insert("statsCollectionEnabled".into(), Value::Boolean(false));
    }
    if let Some((ekey, eiv)) = setup.fairplay {
        d.insert("ekey".into(), Value::Data(ekey.to_vec()));
        d.insert("eiv".into(), Value::Data(eiv.to_vec()));
        d.insert("et".into(), uint(32));
    }
    d
}

pub struct StreamSetup<'a> {
    pub stream_connection_id: u64,
    pub control_port: u16,
    pub latency_samples: u32,
    pub shk: Option<&'a [u8; 32]>,
    pub layout: Layout,
}

pub fn stream_plist(setup: &StreamSetup, profile: Profile) -> Dictionary {
    let mut stream = Dictionary::new();
    stream.insert("type".into(), uint(STREAM_TYPE_REALTIME_AUDIO));
    stream.insert("streamConnectionID".into(), uint(setup.stream_connection_id));
    stream.insert("ct".into(), uint(ALAC_COMPRESSION_TYPE));
    stream.insert("spf".into(), uint(u64::from(crate::rtp::SPF)));
    stream.insert("sr".into(), uint(u64::from(crate::rtp::SAMPLE_RATE)));
    stream.insert("audioFormat".into(), uint(ALAC_AUDIO_FORMAT));
    stream.insert("audioMode".into(), Value::String("default".into()));
    if profile == Profile::OwnTone {
        stream.insert("isMedia".into(), Value::Boolean(true));
        stream.insert("supportsDynamicStreamID".into(), Value::Boolean(false));
        stream.insert("latencyMin".into(), uint(OWNTONE_LATENCY_MIN));
        stream.insert("latencyMax".into(), uint(OWNTONE_LATENCY_MAX));
        stream.insert("controlPort".into(), uint(u64::from(setup.control_port)));
        if let Some(key) = setup.shk {
            stream.insert("shk".into(), Value::Data(key.to_vec()));
        }
    } else {
        stream.insert("usingScreen".into(), Value::Boolean(true));
        stream.insert("isMedia".into(), Value::Boolean(false));
        stream.insert("redundantAudio".into(), uint(2));
        stream.insert("latencyMin".into(), uint(0));
        stream.insert("latencyMax".into(), uint(u64::from(setup.latency_samples)));
        match setup.layout {
            Layout::ControlPort => {
                stream.insert("controlPort".into(), uint(u64::from(setup.control_port)));
                if let Some(key) = setup.shk {
                    stream.insert("shk".into(), Value::Data(key.to_vec()));
                }
            }
            Layout::StreamConnections => {
                stream.insert("supportsDynamicStreamID".into(), Value::Boolean(true));
                if let Some(key) = setup.shk {
                    stream.insert("shk".into(), Value::Data(key.to_vec()));
                }
                let mut rtp = Dictionary::new();
                rtp.insert(
                    "streamConnectionKeyUseStreamEncryptionKey".into(),
                    Value::Boolean(setup.shk.is_some()),
                );
                let mut rtcp = Dictionary::new();
                rtcp.insert("streamConnectionKeyPort".into(), uint(u64::from(setup.control_port)));
                let mut connections = Dictionary::new();
                connections.insert("streamConnectionTypeRTP".into(), Value::Dictionary(rtp));
                connections.insert("streamConnectionTypeRTCP".into(), Value::Dictionary(rtcp));
                stream.insert("streamConnections".into(), Value::Dictionary(connections));
            }
        }
    }
    let mut root = Dictionary::new();
    root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
    root
}

pub fn teardown_plist() -> Dictionary {
    let mut stream = Dictionary::new();
    stream.insert("type".into(), uint(STREAM_TYPE_REALTIME_AUDIO));
    let mut root = Dictionary::new();
    root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
    root
}

pub fn to_binary(dict: Dictionary) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &Value::Dictionary(dict))?;
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetupReply {
    pub timing_port: u16,
    pub event_port: u16,
    pub data_port: u16,
    pub control_port: u16,
}

fn port(dict: &Dictionary, key: &str) -> u16 {
    dict.get(key)
        .and_then(Value::as_unsigned_integer)
        .and_then(|v| u16::try_from(v).ok())
        .unwrap_or(0)
}

pub fn parse_setup_reply(body: &[u8]) -> Result<SetupReply> {
    if body.is_empty() {
        return Ok(SetupReply::default());
    }
    let value = Value::from_reader(std::io::Cursor::new(body))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| Error::protocol("la respuesta de SETUP no es un diccionario"))?;
    let mut reply = SetupReply {
        timing_port: port(dict, "timingPort"),
        event_port: port(dict, "eventPort"),
        ..SetupReply::default()
    };
    if let Some(stream) = dict
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_dictionary)
    {
        reply.data_port = port(stream, "dataPort");
        reply.control_port = port(stream, "controlPort");
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Dictionary {
        session_plist(&SessionSetup {
            device_id: "AA:BB:CC:DD:EE:FF",
            session_uuid: "UUID",
            source_version: MODERN_SOURCE_VERSION,
            timing_port: 50_000,
            name: "PC",
            fairplay: None,
        }, Profile::Doubletake)
    }

    #[test]
    fn session_plist_declares_ntp_timing() {
        let d = session();
        assert_eq!(d["timingProtocol"].as_string(), Some("NTP"));
        assert_eq!(d["timingPort"].as_unsigned_integer(), Some(50_000));
        assert_eq!(d["deviceID"].as_string(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(d["sessionUUID"].as_string(), Some("UUID"));
        assert!(!d.contains_key("ekey"));
    }

    #[test]
    fn session_plist_carries_fairplay_material() {
        let ekey = [1u8; 72];
        let eiv = [2u8; 16];
        let d = session_plist(&SessionSetup {
            device_id: "x",
            session_uuid: "y",
            source_version: LEGACY_SOURCE_VERSION,
            timing_port: 1,
            name: "n",
            fairplay: Some((&ekey, &eiv)),
        }, Profile::Doubletake);
        assert_eq!(d["ekey"].as_data().map(<[u8]>::len), Some(72));
        assert_eq!(d["eiv"].as_data().map(<[u8]>::len), Some(16));
    }

    fn first_stream(root: &Dictionary) -> &Dictionary {
        root["streams"].as_array().unwrap()[0].as_dictionary().unwrap()
    }

    #[test]
    fn owntone_stream_plist_matches_owntone_fields() {
        let key = [9u8; 32];
        let root = stream_plist(
            &StreamSetup {
                stream_connection_id: 77,
                control_port: 6001,
                latency_samples: 13_230,
                shk: Some(&key),
                layout: Layout::StreamConnections,
            },
            Profile::OwnTone,
        );
        let s = first_stream(&root);
        let mut keys: Vec<&str> = s.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "audioFormat", "audioMode", "controlPort", "ct", "isMedia", "latencyMax", "latencyMin", "shk",
                "spf", "sr", "streamConnectionID", "supportsDynamicStreamID", "type"
            ]
        );
        assert_eq!(s["audioFormat"].as_unsigned_integer(), Some(0x40000));
        assert_eq!(s["audioMode"].as_string(), Some("default"));
        assert_eq!(s["isMedia"].as_boolean(), Some(true));
        assert_eq!(s["supportsDynamicStreamID"].as_boolean(), Some(false));
        assert_eq!(s["latencyMin"].as_unsigned_integer(), Some(11_025));
        assert_eq!(s["latencyMax"].as_unsigned_integer(), Some(88_200));
        assert_eq!(s["controlPort"].as_unsigned_integer(), Some(6001));
        assert_eq!(s["streamConnectionID"].as_unsigned_integer(), Some(77));
        assert_eq!(s["shk"].as_data().map(<[u8]>::len), Some(32));
    }

    #[test]
    fn owntone_session_plist_is_minimal_ntp() {
        let d = session_plist(
            &SessionSetup {
                device_id: "AA:BB:CC:DD:EE:FF",
                session_uuid: "U",
                source_version: MODERN_SOURCE_VERSION,
                timing_port: 5000,
                name: "PC",
                fairplay: None,
            },
            Profile::OwnTone,
        );
        let mut keys: Vec<&str> = d.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["deviceID", "isMultiSelectAirPlay", "senderSupportsRelay", "sessionUUID", "timingPort", "timingProtocol"]
        );
    }

    #[test]
    fn control_port_stream_plist() {
        let key = [9u8; 32];
        let root = stream_plist(&StreamSetup {
            stream_connection_id: 77,
            control_port: 6001,
            latency_samples: 13_230,
            shk: Some(&key),
            layout: Layout::ControlPort,
        }, Profile::Doubletake);
        let s = first_stream(&root);
        assert_eq!(s["type"].as_unsigned_integer(), Some(96));
        assert_eq!(s["ct"].as_unsigned_integer(), Some(2));
        assert_eq!(s["spf"].as_unsigned_integer(), Some(352));
        assert_eq!(s["audioFormat"].as_unsigned_integer(), Some(0x40000));
        assert_eq!(s["controlPort"].as_unsigned_integer(), Some(6001));
        assert_eq!(s["latencyMin"].as_unsigned_integer(), Some(0));
        assert_eq!(s["latencyMax"].as_unsigned_integer(), Some(13_230));
        assert_eq!(s["shk"].as_data().map(<[u8]>::len), Some(32));
        assert!(!s.contains_key("streamConnections"));
    }

    #[test]
    fn stream_connections_layout_moves_ports_and_key_flag() {
        let key = [9u8; 32];
        let root = stream_plist(&StreamSetup {
            stream_connection_id: 1,
            control_port: 6002,
            latency_samples: 1,
            shk: Some(&key),
            layout: Layout::StreamConnections,
        }, Profile::Doubletake);
        let s = first_stream(&root);
        assert!(!s.contains_key("controlPort"));
        let connections = s["streamConnections"].as_dictionary().unwrap();
        let rtcp = connections["streamConnectionTypeRTCP"].as_dictionary().unwrap();
        assert_eq!(rtcp["streamConnectionKeyPort"].as_unsigned_integer(), Some(6002));
        let rtp = connections["streamConnectionTypeRTP"].as_dictionary().unwrap();
        assert_eq!(rtp["streamConnectionKeyUseStreamEncryptionKey"].as_boolean(), Some(true));
        assert_eq!(Layout::StreamConnections.other(), Layout::ControlPort);
    }

    #[test]
    fn aes_stream_has_no_key() {
        let root = stream_plist(&StreamSetup {
            stream_connection_id: 1,
            control_port: 1,
            latency_samples: 1,
            shk: None,
            layout: Layout::ControlPort,
        }, Profile::Doubletake);
        assert!(!first_stream(&root).contains_key("shk"));
    }

    #[test]
    fn binary_roundtrip_and_reply_parsing() {
        let mut stream = Dictionary::new();
        stream.insert("dataPort".into(), uint(7001));
        stream.insert("controlPort".into(), uint(7002));
        let mut root = Dictionary::new();
        root.insert("timingPort".into(), uint(7003));
        root.insert("eventPort".into(), uint(7004));
        root.insert("streams".into(), Value::Array(vec![Value::Dictionary(stream)]));
        let body = to_binary(root).unwrap();
        assert_eq!(
            parse_setup_reply(&body).unwrap(),
            SetupReply {
                timing_port: 7003,
                event_port: 7004,
                data_port: 7001,
                control_port: 7002
            }
        );
        assert_eq!(parse_setup_reply(&[]).unwrap(), SetupReply::default());
    }

    #[test]
    fn teardown_names_the_audio_stream() {
        let root = teardown_plist();
        assert_eq!(first_stream(&root)["type"].as_unsigned_integer(), Some(96));
    }
}
