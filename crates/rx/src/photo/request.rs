// Parsing of the photo and slideshow requests: headers of PUT /photo, the slideshow plist and
// the asset bodies (a bare picture or a plist that wraps it).
use std::time::Duration;

use plist::Value;
use rustplay_core::rtsp::Request;

pub const ASSET_KEY: &str = "X-Apple-AssetKey";
pub const ASSET_ACTION: &str = "X-Apple-AssetAction";
pub const TRANSITION: &str = "X-Apple-Transition";

pub const DEFAULT_SLIDE: Duration = Duration::from_secs(3);
const MIN_SLIDE: Duration = Duration::from_secs(1);
const MAX_SLIDE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    // Show it (and keep it for later).
    Display,
    // Keep it for a later `DisplayCached`.
    CacheOnly,
    // Show a picture sent earlier.
    DisplayCached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoRequest {
    pub key: Option<String>,
    pub action: Action,
    pub fade: bool,
}

pub fn parse_photo(request: &Request) -> PhotoRequest {
    let action = match request.headers.get(ASSET_ACTION).map(str::trim) {
        Some(a) if a.eq_ignore_ascii_case("cacheOnly") => Action::CacheOnly,
        Some(a) if a.eq_ignore_ascii_case("displayCached") => Action::DisplayCached,
        _ => Action::Display,
    };
    // Cube, Dissolve, FlipLeft... all become a cross-fade; only "None" is instant.
    let fade = !request.headers.get(TRANSITION).is_some_and(|t| t.trim().eq_ignore_ascii_case("none"));
    PhotoRequest { key: request.headers.get(ASSET_KEY).map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()), action, fade }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slideshow {
    pub duration: Duration,
    pub theme: String,
    pub state: Option<State>,
}

impl Slideshow {
    pub fn ken_burns(&self) -> bool {
        let theme: String = self.theme.chars().filter(char::is_ascii_alphanumeric).collect();
        theme.eq_ignore_ascii_case("kenburns")
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Real(r) => Some(*r),
        Value::Integer(i) => i.as_signed().map(|v| v as f64).or_else(|| i.as_unsigned().map(|v| v as f64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

// `PUT /slideshows/1`: {settings: {slideDuration, theme, state}}. Some clients put the settings
// at the top level, so both are accepted.
pub fn parse_slideshow(body: &[u8]) -> Option<Slideshow> {
    let root = Value::from_reader(std::io::Cursor::new(body)).ok()?.into_dictionary()?;
    let settings = root.get("settings").and_then(Value::as_dictionary).unwrap_or(&root);
    let seconds = settings.get("slideDuration").and_then(number).filter(|s| s.is_finite()).unwrap_or(DEFAULT_SLIDE.as_secs_f64());
    let duration = Duration::from_secs_f64(seconds.max(0.0)).clamp(MIN_SLIDE, MAX_SLIDE);
    let theme = settings.get("theme").and_then(Value::as_string).unwrap_or("Classic").to_owned();
    let state = settings.get("state").and_then(Value::as_string).map(|s| match s.to_ascii_lowercase().as_str() {
        "playing" => State::Playing,
        "paused" => State::Paused,
        _ => State::Stopped,
    });
    Some(Slideshow { duration, theme, state })
}

// An asset body: a picture as is, or a plist with the picture in `data`.
pub fn asset_bytes(body: &[u8]) -> Option<Vec<u8>> {
    if body.is_empty() {
        return None;
    }
    if body.starts_with(b"bplist") || body.starts_with(b"<?xml") {
        let root = Value::from_reader(std::io::Cursor::new(body)).ok()?.into_dictionary()?;
        let data = root.get("data").and_then(Value::as_data)?;
        return (!data.is_empty()).then(|| data.to_vec());
    }
    Some(body.to_vec())
}

// Asset number of `/slideshows/1/assets/<n>`.
pub fn asset_number(path: &str) -> Option<u32> {
    path.strip_prefix("/slideshows/1/assets/")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use plist::Dictionary;
    use rustplay_core::rtsp::{Headers, Protocol};

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut h = Headers::default();
        for (k, v) in headers {
            h.set(*k, *v);
        }
        Request { method: "PUT".into(), uri: "/photo".into(), protocol: Protocol::Http11, headers: h, body: Bytes::new() }
    }

    #[test]
    fn reads_photo_headers() {
        let p = parse_photo(&request(&[(ASSET_KEY, "K-1"), (ASSET_ACTION, "cacheOnly"), (TRANSITION, "Dissolve")]));
        assert_eq!(p, PhotoRequest { key: Some("K-1".into()), action: Action::CacheOnly, fade: true });
        let p = parse_photo(&request(&[(ASSET_KEY, "K-1"), (ASSET_ACTION, "displayCached"), (TRANSITION, "None")]));
        assert_eq!((p.action, p.fade), (Action::DisplayCached, false));
        let p = parse_photo(&request(&[]));
        assert_eq!((p.key, p.action, p.fade), (None, Action::Display, true));
    }

    fn slideshow_plist(settings: Dictionary, nested: bool) -> Vec<u8> {
        let mut root = Dictionary::new();
        if nested {
            root.insert("settings".into(), Value::Dictionary(settings));
        } else {
            root = settings;
        }
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &Value::Dictionary(root)).unwrap();
        out
    }

    #[test]
    fn reads_slideshow_settings() {
        let mut s = Dictionary::new();
        s.insert("slideDuration".into(), Value::Integer(5.into()));
        s.insert("theme".into(), Value::String("Ken Burns".into()));
        s.insert("state".into(), Value::String("playing".into()));
        for nested in [true, false] {
            let show = parse_slideshow(&slideshow_plist(s.clone(), nested)).unwrap();
            assert_eq!((show.duration, show.state), (Duration::from_secs(5), Some(State::Playing)));
            assert!(show.ken_burns());
        }
        let mut odd = Dictionary::new();
        odd.insert("slideDuration".into(), Value::Real(9999.0));
        let show = parse_slideshow(&slideshow_plist(odd, true)).unwrap();
        assert_eq!((show.duration, show.theme.as_str(), show.state), (MAX_SLIDE, "Classic", None));
        assert!(!show.ken_burns());
        assert!(parse_slideshow(b"junk").is_none());
    }

    #[test]
    fn unwraps_asset_bodies() {
        assert_eq!(asset_bytes(b"\xFF\xD8jpeg"), Some(b"\xFF\xD8jpeg".to_vec()));
        assert_eq!(asset_bytes(b""), None);
        let mut d = Dictionary::new();
        d.insert("data".into(), Value::Data(vec![1, 2, 3]));
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &Value::Dictionary(d)).unwrap();
        assert_eq!(asset_bytes(&out), Some(vec![1, 2, 3]));
        assert_eq!(asset_number("/slideshows/1/assets/12"), Some(12));
        assert_eq!(asset_number("/slideshows/1"), None);
    }
}
