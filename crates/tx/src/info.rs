use std::collections::HashMap;

use rustplay_core::{Error, Result};
use plist::{Dictionary, Value};

pub const FEATURE_SCREEN: u32 = 7;
pub const FEATURE_SCREEN_HD: u32 = 28;
pub const FEATURE_FAIRPLAY: u32 = 14;
pub const FEATURE_LEGACY_PAIRING: u32 = 27;
pub const FEATURE_PTP: u32 = 41;
pub const FEATURE_STREAM_CONNECTIONS: u32 = 59;

const STATUS_PASSWORD: u64 = 1 << 7;
const STATUS_PIN: u64 = 1 << 9;

#[derive(Debug, Clone, Default)]
pub struct ReceiverInfo {
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub device_id: String,
    pub source_version: String,
    pub protocol_version: String,
    pub features: u64,
    pub status_flags: u64,
    pub public_key: Option<[u8; 32]>,
    pub audio_stream_formats: Option<u64>,
    pub screen_stream_formats: Option<u64>,
    pub display: Option<(u32, u32)>,
    pub has_ptp_info: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingKind {
    Hap,
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    None,
    Pin,
    Password,
}

fn integer(value: &Value) -> Option<u64> {
    match value {
        Value::Integer(i) => i.as_unsigned().or_else(|| i.as_signed().map(|s| s as u64)),
        Value::Real(r) => Some(*r as u64),
        _ => None,
    }
}

fn string(dict: &Dictionary, key: &str) -> String {
    dict.get(key)
        .and_then(Value::as_string)
        .unwrap_or_default()
        .to_owned()
}

// First advertised display in pixels (widthPixels/heightPixels, else width/height).
fn display_size(dict: &Dictionary) -> Option<(u32, u32)> {
    let display = dict.get("displays")?.as_array()?.first()?.as_dictionary()?;
    let read = |key: &str| display.get(key).and_then(integer).filter(|&v| v > 0);
    let (w, h) = match (read("widthPixels"), read("heightPixels")) {
        (Some(w), Some(h)) => (w, h),
        _ => (read("width")?, read("height")?),
    };
    Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
}

fn parse_hex(text: &str) -> u64 {
    let text = text.trim();
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u64::from_str_radix(text, 16).unwrap_or(0)
}

// mDNS "features" is "0xLOW,0xHIGH" or "0xLOW".
pub fn parse_txt_features(text: &str) -> u64 {
    parse_hex(text.split(',').next().unwrap_or_default())
}

pub fn parse_txt_flags(text: &str) -> u64 {
    parse_hex(text)
}

impl ReceiverInfo {
    pub fn parse(body: &[u8], txt: &HashMap<String, String>) -> Result<Self> {
        let value = Value::from_reader(std::io::Cursor::new(body))?;
        let dict = value
            .as_dictionary()
            .ok_or_else(|| Error::protocol("la respuesta de /info no es un diccionario"))?;
        Ok(Self::from_dictionary(dict, txt))
    }

    pub fn from_dictionary(dict: &Dictionary, txt: &HashMap<String, String>) -> Self {
        let mut info = Self {
            name: string(dict, "name"),
            model: string(dict, "model"),
            manufacturer: string(dict, "manufacturer"),
            device_id: string(dict, "deviceID"),
            source_version: string(dict, "sourceVersion"),
            protocol_version: string(dict, "protocolVersion"),
            has_ptp_info: dict.contains_key("PTPInfo"),
            ..Self::default()
        };
        info.features = dict
            .get("features")
            .and_then(integer)
            .or_else(|| txt.get("features").map(|t| parse_txt_features(t)))
            .unwrap_or(0);
        info.status_flags = dict
            .get("statusFlags")
            .and_then(integer)
            .or_else(|| txt.get("flags").map(|t| parse_txt_flags(t)))
            .unwrap_or(0);
        if info.source_version.is_empty() {
            info.source_version = txt.get("srcvers").cloned().unwrap_or_default();
        }
        if info.device_id.is_empty() {
            info.device_id = txt.get("deviceid").cloned().unwrap_or_default();
        }
        info.public_key = match dict.get("pk") {
            Some(Value::Data(bytes)) => bytes.as_slice().try_into().ok(),
            _ => None,
        };
        info.audio_stream_formats = dict
            .get("supportedFormats")
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("audioStream"))
            .and_then(integer);
        info.screen_stream_formats = dict
            .get("supportedFormats")
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("screenStream"))
            .and_then(integer);
        info.display = display_size(dict);
        info
    }

    // Display metadata only exists once a media session has been created.
    pub fn adopt_session_info(&mut self, dict: &Dictionary) {
        if let Some(size) = display_size(dict) {
            self.display = Some(size);
        }
        if let Some(mask) = dict
            .get("supportedFormats")
            .and_then(Value::as_dictionary)
            .and_then(|d| d.get("screenStream"))
            .and_then(integer)
        {
            self.screen_stream_formats = Some(mask);
        }
    }

    // doubletake's MirrorSize: the advertised display, else 1920x1080 with feature 28 (1280x720
    // without it) for screen receivers; 1920x1080 when nothing is known.
    pub fn mirror_size(&self) -> (u32, u32) {
        if let Some(size) = self.display {
            return size;
        }
        if self.has_feature(FEATURE_SCREEN) && !self.has_feature(FEATURE_SCREEN_HD) {
            return (1280, 720);
        }
        (1920, 1080)
    }

    pub fn supports_screen(&self) -> bool {
        self.has_feature(FEATURE_SCREEN)
    }

    // An absent or zero mask means ALAC, as in doubletake's screenAudioCodec.
    pub fn supports_screen_alac(&self) -> bool {
        match self.screen_stream_formats {
            Some(0) | None => true,
            Some(mask) => mask & 0x40000 != 0,
        }
    }

    pub fn has_feature(&self, bit: u32) -> bool {
        bit < 64 && self.features & (1u64 << bit) != 0
    }

    pub fn requires_password(&self) -> bool {
        self.status_flags & STATUS_PASSWORD != 0
    }

    pub fn requires_pin_flag(&self) -> bool {
        self.status_flags & STATUS_PIN != 0
    }

    pub fn uses_modern_pairing(&self) -> bool {
        let core = [38, 43, 46, 48].into_iter().any(|b| self.has_feature(b));
        let third_party = self.has_feature(26) || self.has_feature(51);
        core && !third_party
    }

    pub fn pairing_kind(&self) -> PairingKind {
        if self.uses_modern_pairing() {
            PairingKind::Hap
        } else {
            PairingKind::Legacy
        }
    }

    pub fn supports_transient(&self) -> bool {
        self.has_feature(43) || self.has_feature(48)
    }

    pub fn credential(&self) -> Credential {
        if self.requires_password() {
            return if self.uses_modern_pairing() {
                Credential::None
            } else {
                Credential::Password
            };
        }
        if self.requires_pin_flag() {
            Credential::Pin
        } else {
            Credential::None
        }
    }

    pub fn supports_fairplay(&self) -> bool {
        self.has_feature(FEATURE_FAIRPLAY)
    }

    pub fn supports_legacy_pairing(&self) -> bool {
        self.has_feature(FEATURE_LEGACY_PAIRING)
    }

    pub fn supports_alac(&self) -> bool {
        match self.audio_stream_formats {
            Some(0) | None => true,
            Some(mask) => mask & 0x40000 != 0,
        }
    }

    pub fn ptp_advertised(&self) -> bool {
        self.has_feature(FEATURE_PTP) && source_version_at_least(&self.source_version, (354, 54, 6))
    }

    pub fn describe(&self) -> String {
        format!(
            "modelo «{}», fabricante «{}», versión {}, características 0x{:x}, estado 0x{:x}",
            self.model, self.manufacturer, self.source_version, self.features, self.status_flags
        )
    }
}

pub fn parse_source_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    parts.next().is_none().then_some((major, minor, patch))
}

pub fn source_version_at_least(text: &str, minimum: (u32, u32, u32)) -> bool {
    match parse_source_version(text) {
        Some((377, 40, _)) | None => false,
        Some(version) => version >= minimum,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    Aes,
    ChaCha,
}

pub fn audio_security(encrypted: bool) -> SecurityMode {
    if encrypted {
        SecurityMode::ChaCha
    } else {
        SecurityMode::Aes
    }
}

// Feature 59 advertises the streamConnections descriptor; the alternate shape is retried on rejection.
pub fn uses_stream_connections(info: &ReceiverInfo) -> bool {
    info.has_feature(FEATURE_STREAM_CONNECTIONS)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn dict(entries: &[(&str, Value)]) -> Dictionary {
        let mut d = Dictionary::new();
        for (k, v) in entries {
            d.insert((*k).to_owned(), v.clone());
        }
        d
    }

    pub fn apple_tv() -> ReceiverInfo {
        let features: u64 = (1 << 38) | (1 << 41) | (1 << 43) | (1 << 48) | (1 << 59) | (1 << 14) | (1 << 61);
        let d = dict(&[
            ("name", Value::String("Salón".into())),
            ("model", Value::String("AppleTV14,1".into())),
            ("manufacturer", Value::String("Apple Inc.".into())),
            ("deviceID", Value::String("AA:BB:CC:DD:EE:FF".into())),
            ("sourceVersion", Value::String("770.8.1".into())),
            ("features", Value::Integer(features.into())),
            ("statusFlags", Value::Integer(0x4u64.into())),
            ("pk", Value::Data(vec![7; 32])),
        ]);
        ReceiverInfo::from_dictionary(&d, &HashMap::new())
    }

    pub fn legacy_receiver() -> ReceiverInfo {
        let features: u64 = (1 << 27) | (1 << 14) | (1 << 9) | (1 << 19);
        let d = dict(&[
            ("name", Value::String("Receptor".into())),
            ("model", Value::String("AirReceiver".into())),
            ("deviceID", Value::String("11:22:33:44:55:66".into())),
            ("sourceVersion", Value::String("220.68".into())),
            ("features", Value::Integer(features.into())),
            ("statusFlags", Value::Integer(0x4u64.into())),
        ]);
        ReceiverInfo::from_dictionary(&d, &HashMap::new())
    }

    // Values captured with GET /info from a Samsung Q60DD (Tizen, AirTunes/377.40.00).
    pub fn samsung_body() -> Vec<u8> {
        let d = dict(&[
            ("name", Value::String("55\" QLED".into())),
            ("model", Value::String("Q60DD".into())),
            ("manufacturer", Value::String("Samsung".into())),
            ("integrator", Value::String("Samsung".into())),
            ("operatingSystem", Value::String("Tizen".into())),
            ("deviceID", Value::String("B0:E4:5C:C3:5A:00".into())),
            ("pi", Value::String("02:12:8A:3B:F2:1A".into())),
            ("protocolVersion", Value::String("1.1".into())),
            ("sourceVersion", Value::String("377.40.00".into())),
            ("features", Value::Integer(255_525_703_439_583_952u64.into())),
            ("featuresEx", Value::String("0Ip/AEbPiwNACA".into())),
            ("statusFlags", Value::Integer(580u64.into())),
            ("volumeControlType", Value::Integer(3u64.into())),
            ("keepAliveSendStatsAsBody", Value::Boolean(true)),
            ("PTPInfo", Value::String("AirPlay;3.6.0.72".into())),
        ]);
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(d)).unwrap();
        body
    }

    pub fn pin_receiver() -> ReceiverInfo {
        let mut info = apple_tv();
        info.status_flags |= STATUS_PIN;
        info
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn apple_tv_uses_hap_and_modern_layout() {
        let info = apple_tv();
        assert_eq!(info.pairing_kind(), PairingKind::Hap);
        assert!(info.supports_transient());
        assert!(uses_stream_connections(&info));
        assert!(info.ptp_advertised());
        assert_eq!(info.credential(), Credential::None);
        assert_eq!(info.public_key, Some([7; 32]));
    }

    #[test]
    fn samsung_q60dd_profile() {
        let info = ReceiverInfo::parse(&samsung_body(), &HashMap::new()).unwrap();
        assert_eq!(info.model, "Q60DD");
        assert_eq!(info.source_version, "377.40.00");
        assert_eq!(info.features, 0x038B_CF46_007F_8AD0);
        // Core HAP bits (38, 43, 46, 48) plus feature 51: classified as third-party/legacy pairing.
        assert!([38, 43, 46, 48, 51].iter().all(|b| info.has_feature(*b)));
        assert_eq!(info.pairing_kind(), PairingKind::Legacy);
        // statusFlags 0x244: PIN pairing demanded, no password.
        assert!(info.requires_pin_flag() && !info.requires_password());
        assert_eq!(info.credential(), Credential::Pin);
        // PTP bit 41 is set but SourceVersion 377.40.x is excluded: NTP timing.
        assert!(info.has_feature(FEATURE_PTP) && !info.ptp_advertised());
        assert!(!uses_stream_connections(&info));
        assert!(!info.supports_fairplay());
        assert!(info.supports_alac());
        assert_eq!(crate::pairing::plan(&info, false), [crate::pairing::Method::Pin]);
    }

    #[test]
    fn pin_flag_requires_pin() {
        assert_eq!(pin_receiver().credential(), Credential::Pin);
    }

    #[test]
    fn legacy_receiver_is_not_hap() {
        let info = legacy_receiver();
        assert_eq!(info.pairing_kind(), PairingKind::Legacy);
        assert!(info.supports_fairplay());
        assert!(!info.ptp_advertised());
        assert!(!uses_stream_connections(&info));
    }

    #[test]
    fn third_party_bits_force_legacy_even_with_core_bits() {
        let mut info = apple_tv();
        info.features |= 1 << 26;
        assert_eq!(info.pairing_kind(), PairingKind::Legacy);
    }

    #[test]
    fn password_is_digest_only_for_modern_receivers() {
        let mut info = apple_tv();
        info.status_flags |= STATUS_PASSWORD;
        assert_eq!(info.credential(), Credential::None);
        let mut legacy = legacy_receiver();
        legacy.status_flags |= STATUS_PASSWORD;
        assert_eq!(legacy.credential(), Credential::Password);
    }

    #[test]
    fn source_version_comparison() {
        assert!(source_version_at_least("354.54.6", (354, 54, 6)));
        assert!(source_version_at_least("770.8.1", (354, 54, 6)));
        assert!(!source_version_at_least("354.54.5", (354, 54, 6)));
        assert!(!source_version_at_least("377.40.1", (354, 54, 6)));
        assert!(!source_version_at_least("abc", (354, 54, 6)));
        assert_eq!(parse_source_version("220.68"), Some((220, 68, 0)));
    }

    #[test]
    fn txt_features_fall_back() {
        assert_eq!(parse_txt_features("0x445F8A00,0x1C300"), 0x445F_8A00);
        assert_eq!(parse_txt_features("0x4A7FFFF7"), 0x4A7F_FFF7);
        assert_eq!(parse_txt_features("zz"), 0);
        let mut txt = HashMap::new();
        txt.insert("features".to_owned(), "0x4000".to_owned());
        txt.insert("flags".to_owned(), "0x204".to_owned());
        let info = ReceiverInfo::from_dictionary(&Dictionary::new(), &txt);
        assert_eq!(info.features, 0x4000);
        assert_eq!(info.status_flags, 0x204);
        assert!(info.requires_pin_flag());
    }

    #[test]
    fn parses_binary_plist_body() {
        let mut dict = Dictionary::new();
        dict.insert("name".into(), Value::String("TV".into()));
        dict.insert("features".into(), Value::Integer((1u64 << 48).into()));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(dict)).unwrap();
        let info = ReceiverInfo::parse(&body, &HashMap::new()).unwrap();
        assert_eq!(info.name, "TV");
        assert!(info.has_feature(48));
    }

    #[test]
    fn security_mode_follows_encryption() {
        assert_eq!(audio_security(true), SecurityMode::ChaCha);
        assert_eq!(audio_security(false), SecurityMode::Aes);
    }
}
