pub const MODEL: &str = "AppleTV3,2";
pub const SOURCE_VERSION: &str = "220.68";
pub const FEATURES_LOW: u32 = 0x5A7F_FEE6;
pub const FEATURES_HIGH: u32 = 0x0;
// Feature bits 0 (video) and 4 (HLS), advertised only when receiving videos from apps is on.
pub const FEATURES_VIDEO: u32 = 0x11;
// Feature bits 1 (photo), 5 (slideshow) and 13 (photo preloading). The base set already carries
// them (as UxPlay does), so turning photos on changes nothing on the wire; the constant makes
// sure they stay advertised if the base set ever changes.
pub const FEATURES_PHOTO: u32 = 0x2022;
pub const STATUS_FLAGS: u64 = 0x44;

pub type Txt = Vec<(String, String)>;

pub struct TxtRecords {
    pub raop: Txt,
    pub airplay: Txt,
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0x0F)] as char);
    }
    out
}

pub fn device_id_colon(device_id: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        device_id[0], device_id[1], device_id[2], device_id[3], device_id[4], device_id[5]
    )
}

pub fn raop_instance(device_id: &[u8; 6], name: &str) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}@{name}",
        device_id[0], device_id[1], device_id[2], device_id[3], device_id[4], device_id[5]
    )
}

pub fn features_low_with(video: bool, photos: bool) -> u32 {
    let mut features = FEATURES_LOW;
    if video {
        features |= FEATURES_VIDEO;
    }
    if photos {
        features |= FEATURES_PHOTO;
    }
    features
}

pub fn features_string_with(video: bool, photos: bool) -> String {
    format!("0x{:X},0x{FEATURES_HIGH:X}", features_low_with(video, photos))
}

#[cfg(test)]
pub fn build(device_id: &[u8; 6], public_key: &[u8; 32], pairing_id: &str) -> TxtRecords {
    build_with(device_id, public_key, pairing_id, false)
}

#[cfg(test)]
pub fn build_with(device_id: &[u8; 6], public_key: &[u8; 32], pairing_id: &str, video: bool) -> TxtRecords {
    build_full(device_id, public_key, pairing_id, video, false)
}

pub fn build_full(device_id: &[u8; 6], public_key: &[u8; 32], pairing_id: &str, video: bool, photos: bool) -> TxtRecords {
    let features = features_string_with(video, photos);
    let pk = hex(public_key);
    let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());

    let raop = vec![
        pair("ch", "2"),
        pair("cn", "0,1,2,3"),
        pair("da", "true"),
        pair("et", "0,3,5"),
        pair("vv", "2"),
        pair("ft", &features),
        pair("am", MODEL),
        pair("md", "0,1,2"),
        pair("rhd", "5.6.0.0"),
        pair("pw", "false"),
        pair("sf", "0x4"),
        pair("sr", "44100"),
        pair("ss", "16"),
        pair("sv", "false"),
        pair("tp", "UDP"),
        pair("txtvers", "1"),
        pair("vs", SOURCE_VERSION),
        pair("vn", "65537"),
        pair("pk", &pk),
    ];
    let airplay = vec![
        pair("deviceid", &device_id_colon(device_id)),
        pair("features", &features),
        pair("pw", "false"),
        pair("flags", "0x4"),
        pair("model", MODEL),
        pair("pk", &pk),
        pair("pi", pairing_id),
        pair("srcvers", SOURCE_VERSION),
        pair("vv", "2"),
    ];
    TxtRecords { raop, airplay }
}

// PIN pairing, as UxPlay's `-pin`: `pw=true` everywhere and status flag 0x8 ("PIN required")
// in the RAOP record.
pub fn require_pin(records: &mut TxtRecords) {
    for (key, value) in records.raop.iter_mut().chain(records.airplay.iter_mut()) {
        match key.as_str() {
            "pw" => *value = String::from("true"),
            "sf" => *value = String::from("0x8c"),
            _ => {}
        }
    }
}

pub fn wire_format(txt: &Txt) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in txt {
        let Ok(len) = u8::try_from(k.len() + 1 + v.len()) else {
            continue;
        };
        out.push(len);
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(txt: &'a Txt, key: &str) -> Option<&'a str> {
        txt.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn builds_uxplay_records() {
        let id = [0x00, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E];
        let records = build(&id, &[0xAB; 32], "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE");
        assert_eq!(get(&records.airplay, "deviceid"), Some("00:1A:2B:3C:4D:5E"));
        assert_eq!(get(&records.airplay, "features"), Some("0x5A7FFEE6,0x0"));
        assert_eq!(get(&records.airplay, "flags"), Some("0x4"));
        assert_eq!(get(&records.airplay, "model"), Some("AppleTV3,2"));
        assert_eq!(
            get(&records.airplay, "pi"),
            Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
        );
        assert_eq!(get(&records.airplay, "pk").map(str::len), Some(64));
        assert_eq!(get(&records.airplay, "srcvers"), Some("220.68"));
        assert_eq!(get(&records.airplay, "vv"), Some("2"));
        for key in [
            "ch", "cn", "da", "et", "ft", "md", "am", "pk", "sf", "sr", "ss", "sv", "tp",
            "txtvers", "vn", "vs", "vv",
        ] {
            assert!(get(&records.raop, key).is_some(), "falta {key}");
        }
        assert_eq!(get(&records.raop, "ft"), Some("0x5A7FFEE6,0x0"));
        assert_eq!(get(&records.raop, "et"), Some("0,3,5"));
        assert_eq!(get(&records.raop, "pk"), get(&records.airplay, "pk"));
    }

    #[test]
    fn pin_flags() {
        let mut records = build(&[0; 6], &[0; 32], "P");
        assert_eq!(get(&records.raop, "pw"), Some("false"));
        require_pin(&mut records);
        assert_eq!(get(&records.raop, "pw"), Some("true"));
        assert_eq!(get(&records.raop, "sf"), Some("0x8c"));
        assert_eq!(get(&records.airplay, "pw"), Some("true"));
        assert_eq!(get(&records.airplay, "flags"), Some("0x4"));
    }

    #[test]
    fn video_feature_bits_are_advertised_only_when_enabled() {
        let id = [0; 6];
        let off = build_with(&id, &[1; 32], "P", false);
        let on = build_with(&id, &[1; 32], "P", true);
        assert_eq!(off.airplay, build(&id, &[1; 32], "P").airplay);
        assert_eq!(get(&off.airplay, "features"), Some("0x5A7FFEE6,0x0"));
        assert_eq!(get(&on.airplay, "features"), Some("0x5A7FFEF7,0x0"));
        assert_eq!(get(&on.raop, "ft"), Some("0x5A7FFEF7,0x0"));
    }

    #[test]
    fn photo_setting_keeps_the_records_identical() {
        let id = [1, 2, 3, 4, 5, 6];
        for video in [false, true] {
            let off = build_full(&id, &[7; 32], "P", video, false);
            let on = build_full(&id, &[7; 32], "P", video, true);
            assert_eq!(off.raop, on.raop);
            assert_eq!(off.airplay, on.airplay);
        }
        assert_eq!(build_full(&id, &[7; 32], "P", false, false).airplay, build(&id, &[7; 32], "P").airplay);
        // Photo, slideshow and preloading are advertised either way.
        assert_eq!(FEATURES_LOW & FEATURES_PHOTO, FEATURES_PHOTO);
    }

    #[test]
    fn instance_and_hex() {
        assert_eq!(
            raop_instance(&[0, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E], "PC"),
            "001A2B3C4D5E@PC"
        );
        assert_eq!(hex(&[0x0F, 0xA0, 0xFF]), "0fa0ff");
    }

    #[test]
    fn wire_format_is_length_prefixed() {
        let wire = wire_format(&vec![("a".to_owned(), "bc".to_owned())]);
        assert_eq!(wire, b"\x04a=bc");
    }
}
