use rustplay_core::media::audio::AudioCodec;
use rustplay_core::{Error, Result};
use plist::Value;

pub const DEFAULT_SAMPLE_RATE: u32 = 44_100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRequest {
    pub ct: u8,
    pub spf: u32,
    pub sample_rate: u32,
    pub control_port: u16,
    pub audio_format: u64,
    pub latency_min: Option<u32>,
}

impl AudioRequest {
    pub fn codec(&self) -> AudioCodec {
        match self.ct {
            2 => AudioCodec::Alac,
            4 => AudioCodec::AacLc,
            8 => AudioCodec::AacEld,
            _ => AudioCodec::Pcm,
        }
    }

    pub fn frames_per_packet(&self) -> u32 {
        if self.spf != 0 {
            return self.spf;
        }
        match self.codec() {
            AudioCodec::AacLc => 1024,
            AudioCodec::AacEld => 480,
            AudioCodec::Alac | AudioCodec::Pcm => 352,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRequest {
    Audio(AudioRequest),
    Mirror { connection_id: u64 },
    Other(u64),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupRequest {
    pub ekey: Option<Vec<u8>>,
    pub eiv: Option<Vec<u8>>,
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
    pub timing_protocol: Option<String>,
    pub timing_port: u16,
    pub remote_control_only: bool,
    pub streams: Option<Vec<StreamRequest>>,
}

impl SetupRequest {
    pub fn has_session_keys(&self) -> bool {
        self.ekey.is_some() && self.eiv.is_some()
    }

    pub fn uses_ntp(&self) -> bool {
        self.timing_port != 0
            && self
                .timing_protocol
                .as_deref()
                .is_none_or(|p| p.eq_ignore_ascii_case("NTP"))
    }
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_unsigned_integer().or_else(|| {
        value
            .as_signed_integer()
            .and_then(|v| u64::try_from(v).ok())
    })
}

fn wrapping_unsigned(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_unsigned_integer()
        .or_else(|| value.as_signed_integer().map(|v| v as u64))
}

fn text(value: Option<&Value>) -> Option<String> {
    value?.as_string().map(str::to_owned)
}

fn data(value: Option<&Value>) -> Option<Vec<u8>> {
    match value? {
        Value::Data(d) => Some(d.clone()),
        _ => None,
    }
}

fn audio_ct_from_format(audio_format: u64) -> u8 {
    match audio_format {
        0 => 0,
        0x4_0000 | 0x8_0000 => 2,
        0x40_0000 | 0x80_0000 | 0x100_0000 | 0x200_0000 => 8,
        0x400 | 0x800 | 0x1000 | 0x2000 => 4,
        _ => 0,
    }
}

fn parse_stream(value: &Value) -> Option<StreamRequest> {
    let dict = value.as_dictionary()?;
    let kind = unsigned(dict.get("type"))?;
    Some(match kind {
        96 => {
            let audio_format = unsigned(dict.get("audioFormat")).unwrap_or(0);
            let mut ct = unsigned(dict.get("ct")).unwrap_or(0) as u8;
            if ct == 0 {
                ct = audio_ct_from_format(audio_format);
            }
            StreamRequest::Audio(AudioRequest {
                ct,
                spf: unsigned(dict.get("spf")).unwrap_or(0) as u32,
                sample_rate: unsigned(dict.get("sr")).map_or(DEFAULT_SAMPLE_RATE, |v| v as u32),
                control_port: unsigned(dict.get("controlPort")).unwrap_or(0) as u16,
                audio_format,
                latency_min: unsigned(dict.get("latencyMin")).map(|v| v as u32),
            })
        }
        110 => StreamRequest::Mirror {
            connection_id: wrapping_unsigned(dict.get("streamConnectionID")).unwrap_or(0),
        },
        other => StreamRequest::Other(other),
    })
}

pub fn parse_setup(body: &[u8]) -> Result<SetupRequest> {
    if body.is_empty() {
        return Ok(SetupRequest::default());
    }
    let value = Value::from_reader(std::io::Cursor::new(body))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| Error::protocol("el cuerpo de SETUP no es un diccionario"))?;
    let streams = dict
        .get("streams")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_stream).collect());
    Ok(SetupRequest {
        ekey: data(dict.get("ekey")),
        eiv: data(dict.get("eiv")),
        device_id: text(dict.get("deviceID")),
        model: text(dict.get("model")),
        name: text(dict.get("name")),
        timing_protocol: text(dict.get("timingProtocol")),
        timing_port: unsigned(dict.get("timingPort")).unwrap_or(0) as u16,
        remote_control_only: dict
            .get("isRemoteControlOnly")
            .and_then(Value::as_boolean)
            .unwrap_or(false),
        streams,
    })
}

// One request of the legacy PIN pairing (POST /pair-setup-pin, binary plist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinSetup {
    Start { user: String },
    Proof { public_key: Vec<u8>, proof: Vec<u8> },
    Confirm { epk: Vec<u8>, auth_tag: Vec<u8> },
}

pub fn parse_pin_setup(body: &[u8]) -> Result<PinSetup> {
    let value = Value::from_reader(std::io::Cursor::new(body))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| Error::protocol("el cuerpo de pair-setup-pin no es un diccionario"))?;
    if let (Some(method), Some(user)) = (text(dict.get("method")), text(dict.get("user"))) {
        if method != "pin" {
            return Err(Error::Pairing(format!("método de emparejamiento no admitido: {method}")));
        }
        return Ok(PinSetup::Start { user });
    }
    if let (Some(public_key), Some(proof)) = (data(dict.get("pk")), data(dict.get("proof"))) {
        return Ok(PinSetup::Proof { public_key, proof });
    }
    if let (Some(epk), Some(auth_tag)) = (data(dict.get("epk")), data(dict.get("authTag"))) {
        return Ok(PinSetup::Confirm { epk, auth_tag });
    }
    Err(Error::protocol("petición pair-setup-pin desconocida"))
}

pub fn parse_teardown(body: &[u8]) -> Vec<u64> {
    let Ok(value) = Value::from_reader(std::io::Cursor::new(body)) else {
        return Vec::new();
    };
    value
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|s| unsigned(s.as_dictionary()?.get("type")))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextParameter {
    Volume(f32),
    Progress { start: u32, current: u32, end: u32 },
}

pub fn parse_text_parameters(body: &str) -> (Vec<TextParameter>, Vec<String>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for line in body
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        let parsed = if let Some(rest) = line.strip_prefix("volume:") {
            rest.trim().parse::<f32>().ok().map(TextParameter::Volume)
        } else if let Some(rest) = line.strip_prefix("progress:") {
            let mut parts = rest.trim().split('/').map(|p| p.trim().parse::<u32>());
            match (parts.next(), parts.next(), parts.next()) {
                (Some(Ok(start)), Some(Ok(current)), Some(Ok(end))) => {
                    Some(TextParameter::Progress {
                        start,
                        current,
                        end,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        match parsed {
            Some(p) => known.push(p),
            None => unknown.push(line.to_owned()),
        }
    }
    (known, unknown)
}

pub fn requests_volume(body: &str) -> bool {
    body.split(['\r', '\n']).any(|l| l.trim() == "volume")
}

pub fn rtp_info_sequence(header: &str) -> Option<u16> {
    header.split(';').find_map(|part| {
        let value = part.trim().strip_prefix("seq=")?;
        let end = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        value[..end]
            .parse::<u32>()
            .ok()
            .and_then(|v| u16::try_from(v).ok())
    })
}

// iOS sends -144 dB for "muted" and -30 dB at the bottom of its volume slider, which would still
// be audible: the range from -30 to 0 dB is mapped so that the bottom of the slider is silent.
const SLIDER_FLOOR_DB: f32 = -30.0;

pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() || db <= SLIDER_FLOOR_DB + 0.05 {
        return 0.0;
    }
    let gain = 10f32.powf(db.min(0.0) / 20.0);
    let floor = 10f32.powf(SLIDER_FLOOR_DB / 20.0);
    ((gain - floor) / (1.0 - floor)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use plist::Dictionary;

    use super::*;

    fn to_bin(dict: Dictionary) -> Vec<u8> {
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &Value::Dictionary(dict)).unwrap();
        out
    }

    fn uint(v: u64) -> Value {
        Value::Integer(v.into())
    }

    fn phase1() -> Dictionary {
        let mut d = Dictionary::new();
        d.insert("ekey".into(), Value::Data(vec![1; 72]));
        d.insert("eiv".into(), Value::Data(vec![2; 16]));
        d.insert("deviceID".into(), Value::String("AA:BB:CC:DD:EE:FF".into()));
        d.insert("model".into(), Value::String("iPhone15,2".into()));
        d.insert("name".into(), Value::String("iPhone de Ana".into()));
        d.insert("timingProtocol".into(), Value::String("NTP".into()));
        d.insert("timingPort".into(), uint(53_021));
        d.insert("isRemoteControlOnly".into(), Value::Boolean(false));
        d
    }

    fn audio_stream(ct: u64) -> Value {
        let mut s = Dictionary::new();
        s.insert("type".into(), uint(96));
        s.insert("ct".into(), uint(ct));
        s.insert("spf".into(), uint(352));
        s.insert("controlPort".into(), uint(60_001));
        s.insert("audioFormat".into(), uint(0x4_0000));
        s.insert("latencyMin".into(), uint(11_025));
        s.insert("isMedia".into(), Value::Boolean(true));
        Value::Dictionary(s)
    }

    #[test]
    fn pin_setup_steps() {
        let mut d = Dictionary::new();
        d.insert("method".into(), Value::String("pin".into()));
        d.insert("user".into(), Value::String("AA:BB".into()));
        assert_eq!(parse_pin_setup(&to_bin(d)).unwrap(), PinSetup::Start { user: "AA:BB".into() });
        let mut d = Dictionary::new();
        d.insert("pk".into(), Value::Data(vec![1, 2]));
        d.insert("proof".into(), Value::Data(vec![3]));
        assert_eq!(
            parse_pin_setup(&to_bin(d)).unwrap(),
            PinSetup::Proof { public_key: vec![1, 2], proof: vec![3] }
        );
        let mut d = Dictionary::new();
        d.insert("epk".into(), Value::Data(vec![4]));
        d.insert("authTag".into(), Value::Data(vec![5]));
        assert_eq!(
            parse_pin_setup(&to_bin(d)).unwrap(),
            PinSetup::Confirm { epk: vec![4], auth_tag: vec![5] }
        );
        let mut d = Dictionary::new();
        d.insert("method".into(), Value::String("password".into()));
        d.insert("user".into(), Value::String("x".into()));
        assert!(parse_pin_setup(&to_bin(d)).is_err());
        assert!(parse_pin_setup(b"basura").is_err());
    }

    #[test]
    fn mirror_connection_id_accepts_negative_values() {
        for (value, expected) in [(-5i64, (-5i64) as u64), (1_234_567_890_123, 1_234_567_890_123)]
        {
            let mut s = Dictionary::new();
            s.insert("type".into(), uint(110));
            s.insert("streamConnectionID".into(), Value::Integer(value.into()));
            let mut d = Dictionary::new();
            d.insert("streams".into(), Value::Array(vec![Value::Dictionary(s)]));
            let setup = parse_setup(&to_bin(d)).unwrap();
            assert_eq!(
                setup.streams.unwrap(),
                vec![StreamRequest::Mirror {
                    connection_id: expected
                }]
            );
        }
    }

    #[test]
    fn parses_phase_one() {
        let setup = parse_setup(&to_bin(phase1())).unwrap();
        assert!(setup.has_session_keys());
        assert_eq!(setup.ekey.as_ref().unwrap().len(), 72);
        assert_eq!(setup.eiv.as_ref().unwrap().len(), 16);
        assert_eq!(setup.name.as_deref(), Some("iPhone de Ana"));
        assert_eq!(setup.timing_port, 53_021);
        assert!(setup.uses_ntp());
        assert!(setup.streams.is_none());
    }

    #[test]
    fn parses_phase_two_audio_and_mirror() {
        let mut d = Dictionary::new();
        let mut mirror = Dictionary::new();
        mirror.insert("type".into(), uint(110));
        mirror.insert("streamConnectionID".into(), uint(0xFFFF_FFFF_FFFF_FFF0));
        d.insert(
            "streams".into(),
            Value::Array(vec![audio_stream(2), Value::Dictionary(mirror)]),
        );
        let setup = parse_setup(&to_bin(d)).unwrap();
        assert!(!setup.has_session_keys());
        let streams = setup.streams.unwrap();
        let StreamRequest::Audio(audio) = &streams[0] else {
            panic!()
        };
        assert_eq!(audio.codec(), AudioCodec::Alac);
        assert_eq!(audio.frames_per_packet(), 352);
        assert_eq!(audio.sample_rate, 44_100);
        assert_eq!(audio.control_port, 60_001);
        assert_eq!(audio.latency_min, Some(11_025));
        assert_eq!(
            streams[1],
            StreamRequest::Mirror {
                connection_id: 0xFFFF_FFFF_FFFF_FFF0
            }
        );
    }

    #[test]
    fn codec_mapping_and_defaults() {
        let base = AudioRequest {
            ct: 0,
            spf: 0,
            sample_rate: 44_100,
            control_port: 0,
            audio_format: 0,
            latency_min: None,
        };
        let with = |ct| AudioRequest { ct, ..base.clone() };
        assert_eq!(with(2).codec(), AudioCodec::Alac);
        assert_eq!(with(4).codec(), AudioCodec::AacLc);
        assert_eq!(with(4).frames_per_packet(), 1024);
        assert_eq!(with(8).codec(), AudioCodec::AacEld);
        assert_eq!(with(8).frames_per_packet(), 480);
        assert_eq!(with(1).codec(), AudioCodec::Pcm);
        assert_eq!(with(0).codec(), AudioCodec::Pcm);
    }

    #[test]
    fn audio_format_fills_missing_ct() {
        let mut s = Dictionary::new();
        s.insert("type".into(), uint(96));
        s.insert("audioFormat".into(), uint(0x40_0000));
        let mut d = Dictionary::new();
        d.insert("streams".into(), Value::Array(vec![Value::Dictionary(s)]));
        let setup = parse_setup(&to_bin(d)).unwrap();
        let StreamRequest::Audio(a) = &setup.streams.unwrap()[0] else {
            panic!()
        };
        assert_eq!(a.codec(), AudioCodec::AacEld);
    }

    #[test]
    fn rejects_garbage_and_accepts_empty() {
        assert!(parse_setup(b"no es un plist").is_err());
        assert_eq!(parse_setup(b"").unwrap(), SetupRequest::default());
    }

    #[test]
    fn timing_protocol_none_disables_ntp() {
        let mut d = phase1();
        d.insert("timingProtocol".into(), Value::String("None".into()));
        assert!(!parse_setup(&to_bin(d)).unwrap().uses_ntp());
    }

    #[test]
    fn teardown_stream_types() {
        let mut a = Dictionary::new();
        a.insert("type".into(), uint(96));
        let mut d = Dictionary::new();
        d.insert("streams".into(), Value::Array(vec![Value::Dictionary(a)]));
        assert_eq!(parse_teardown(&to_bin(d)), vec![96]);
        assert!(parse_teardown(&to_bin(Dictionary::new())).is_empty());
        assert!(parse_teardown(b"").is_empty());
    }

    #[test]
    fn text_parameters() {
        let (known, unknown) = parse_text_parameters("volume: -20.500000\r\n");
        assert_eq!(known, vec![TextParameter::Volume(-20.5)]);
        assert!(unknown.is_empty());
        let (known, unknown) = parse_text_parameters("progress: 100/2000/90000\r\nfoo: bar\r\n");
        assert_eq!(
            known,
            vec![TextParameter::Progress {
                start: 100,
                current: 2000,
                end: 90_000
            }]
        );
        assert_eq!(unknown, vec!["foo: bar".to_owned()]);
        let (known, unknown) = parse_text_parameters("volume: abc");
        assert!(known.is_empty());
        assert_eq!(unknown.len(), 1);
    }

    #[test]
    fn get_parameter_volume_request() {
        assert!(requests_volume("volume\r\n"));
        assert!(!requests_volume("name\r\n"));
    }

    #[test]
    fn rtp_info() {
        assert_eq!(rtp_info_sequence("seq=1234;rtptime=5678"), Some(1234));
        assert_eq!(rtp_info_sequence("rtptime=5;seq=9"), Some(9));
        assert_eq!(rtp_info_sequence("seq=70000"), None);
        assert_eq!(rtp_info_sequence("nothing"), None);
    }

    #[test]
    fn volume_conversion() {
        assert_eq!(db_to_linear(-144.0), 0.0);
        assert_eq!(db_to_linear(0.0), 1.0);
        assert_eq!(db_to_linear(5.0), 1.0);
        assert!((db_to_linear(-20.0) - (0.1 - 0.031_622_78) / (1.0 - 0.031_622_78)).abs() < 1e-6);
        assert_eq!(db_to_linear(-30.0), 0.0, "the bottom of the slider is silent");
        assert!(db_to_linear(-29.0) > 0.0 && db_to_linear(-29.0) < 0.01);
        assert_eq!(db_to_linear(f32::NAN), 0.0);
    }
}
