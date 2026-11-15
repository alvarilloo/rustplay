// Bodies and query strings of the AirPlay video requests (POST /play, POST /action, /scrub, /rate).
use plist::{Dictionary, Value};
use rustplay_core::media::player::StartAt;
use rustplay_core::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct PlayRequest {
    pub location: String,
    pub start: StartAt,
    pub uuid: Option<String>,
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    // The client's answer to one of our FCUP requests: the playlist found at `url`.
    FcupResponse { url: String, data: Vec<u8>, status: Option<u64>, request_id: Option<u64> },
    PlaylistRemove { uuid: Option<String> },
    PlaylistInsert,
    Other(String),
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_real()
        .or_else(|| value.as_signed_integer().map(|v| v as f64))
        .or_else(|| value.as_unsigned_integer().map(|v| v as f64))
        .filter(|v| v.is_finite())
}

fn is_plist(content_type: Option<&str>, body: &[u8]) -> bool {
    body.starts_with(b"bplist") || body.starts_with(b"<?xml") || content_type.is_some_and(|c| c.contains("plist"))
}

fn dictionary(body: &[u8]) -> Result<Dictionary> {
    Value::from_reader(std::io::Cursor::new(body))?
        .into_dictionary()
        .ok_or_else(|| Error::protocol("el plist no es un diccionario"))
}

fn start_from(seconds: Option<f64>, fraction: Option<f64>) -> StartAt {
    match (seconds, fraction) {
        (Some(s), _) => StartAt::Seconds(s.max(0.0)),
        (None, Some(f)) if f > 0.0 => StartAt::Fraction(f.clamp(0.0, 1.0)),
        _ => StartAt::Seconds(0.0),
    }
}

// POST /play: a binary plist from current iOS, or "Key: value" lines from older clients.
pub fn parse_play(content_type: Option<&str>, body: &[u8]) -> Result<PlayRequest> {
    if is_plist(content_type, body) {
        let dict = dictionary(body)?;
        let text = |key: &str| dict.get(key).and_then(Value::as_string).map(str::to_owned);
        let location = text("Content-Location").filter(|l| !l.is_empty()).ok_or_else(|| Error::protocol("POST /play sin Content-Location"))?;
        let seconds = dict.get("Start-Position-Seconds").and_then(number);
        let fraction = dict.get("Start-Position").and_then(number);
        return Ok(PlayRequest { location, start: start_from(seconds, fraction), uuid: text("uuid"), client: text("clientProcName") });
    }
    let body = String::from_utf8_lossy(body);
    let mut location = None;
    let mut fraction = None;
    for line in body.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "Content-Location" => location = Some(value.trim().to_owned()),
            "Start-Position" => fraction = value.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
            _ => {}
        }
    }
    let location = location.filter(|l| !l.is_empty()).ok_or_else(|| Error::protocol("POST /play sin Content-Location"))?;
    Ok(PlayRequest { location, start: start_from(None, fraction), uuid: None, client: None })
}

// POST /action (always a binary plist with "type" and "params").
pub fn parse_action(body: &[u8]) -> Result<Action> {
    let dict = dictionary(body)?;
    let kind = dict.get("type").and_then(Value::as_string).ok_or_else(|| Error::protocol("POST /action sin tipo"))?;
    let params = dict.get("params").and_then(Value::as_dictionary);
    let item_uuid = || {
        params
            .and_then(|p| p.get("item"))
            .and_then(Value::as_dictionary)
            .and_then(|i| i.get("uuid"))
            .and_then(Value::as_string)
            .map(str::to_owned)
    };
    match kind {
        "unhandledURLResponse" => {
            let params = params.ok_or_else(|| Error::protocol("respuesta FCUP sin parámetros"))?;
            let url = params
                .get("FCUP_Response_URL")
                .and_then(Value::as_string)
                .ok_or_else(|| Error::protocol("respuesta FCUP sin URL"))?
                .to_owned();
            let data = params
                .get("FCUP_Response_Data")
                .and_then(Value::as_data)
                .ok_or_else(|| Error::protocol("respuesta FCUP sin datos"))?
                .to_vec();
            let status = params.get("FCUP_Response_StatusCode").and_then(Value::as_unsigned_integer);
            let request_id = params.get("FCUP_Response_RequestID").and_then(Value::as_unsigned_integer);
            Ok(Action::FcupResponse { url, data, status, request_id })
        }
        "playlistRemove" => Ok(Action::PlaylistRemove { uuid: item_uuid() }),
        "playlistInsert" => Ok(Action::PlaylistInsert),
        other => Ok(Action::Other(other.to_owned())),
    }
}

// The number after "key=" in the query string of `uri` (e.g. /scrub?position=12.5).
pub fn query_number(uri: &str, key: &str) -> Option<f64> {
    let (_, query) = uri.split_once('?')?;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

// The text after '?' (e.g. "actionAtItemEnd" in PUT /setProperty?actionAtItemEnd).
pub fn query(uri: &str) -> &str {
    uri.split_once('?').map_or("", |(_, q)| q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary(dict: Dictionary) -> Vec<u8> {
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &Value::Dictionary(dict)).unwrap();
        out
    }

    #[test]
    fn parses_binary_play_requests() {
        let mut d = Dictionary::new();
        d.insert("Content-Location".into(), Value::String("https://example.com/v.mp4".into()));
        d.insert("Start-Position-Seconds".into(), Value::Real(12.5));
        d.insert("uuid".into(), Value::String("ABC".into()));
        d.insert("clientProcName".into(), Value::String("MobileSafari".into()));
        let play = parse_play(Some("application/x-apple-binary-plist"), &binary(d)).unwrap();
        assert_eq!(play.location, "https://example.com/v.mp4");
        assert_eq!(play.start, StartAt::Seconds(12.5));
        assert_eq!(play.uuid.as_deref(), Some("ABC"));
        assert_eq!(play.client.as_deref(), Some("MobileSafari"));
    }

    #[test]
    fn falls_back_to_the_start_fraction_and_integers() {
        let mut d = Dictionary::new();
        d.insert("Content-Location".into(), Value::String("mlhls://localhost/master.m3u8".into()));
        d.insert("Start-Position".into(), Value::Real(0.25));
        let play = parse_play(None, &binary(d)).unwrap();
        assert_eq!(play.start, StartAt::Fraction(0.25));

        let mut d = Dictionary::new();
        d.insert("Content-Location".into(), Value::String("http://x/y".into()));
        d.insert("Start-Position-Seconds".into(), Value::Integer(30.into()));
        assert_eq!(parse_play(None, &binary(d)).unwrap().start, StartAt::Seconds(30.0));
    }

    #[test]
    fn parses_text_play_requests() {
        let body = b"Content-Location: http://192.168.1.5:7001/1/movie.mp4\nStart-Position: 0.5\n";
        let play = parse_play(Some("text/parameters"), body).unwrap();
        assert_eq!(play.location, "http://192.168.1.5:7001/1/movie.mp4");
        assert_eq!(play.start, StartAt::Fraction(0.5));
    }

    #[test]
    fn rejects_play_requests_without_a_location() {
        assert!(parse_play(Some("text/parameters"), b"Start-Position: 0\n").is_err());
        let mut d = Dictionary::new();
        d.insert("uuid".into(), Value::String("A".into()));
        assert!(parse_play(None, &binary(d)).is_err());
        assert!(parse_play(Some("application/x-apple-binary-plist"), b"bplist00garbage").is_err());
    }

    #[test]
    fn parses_fcup_responses_and_playlist_actions() {
        let mut params = Dictionary::new();
        params.insert("FCUP_Response_URL".into(), Value::String("mlhls://localhost/master.m3u8".into()));
        params.insert("FCUP_Response_Data".into(), Value::Data(b"#EXTM3U\n".to_vec()));
        params.insert("FCUP_Response_StatusCode".into(), Value::Integer(200.into()));
        params.insert("FCUP_Response_RequestID".into(), Value::Integer(3.into()));
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String("unhandledURLResponse".into()));
        d.insert("params".into(), Value::Dictionary(params));
        assert_eq!(
            parse_action(&binary(d)).unwrap(),
            Action::FcupResponse {
                url: "mlhls://localhost/master.m3u8".into(),
                data: b"#EXTM3U\n".to_vec(),
                status: Some(200),
                request_id: Some(3),
            }
        );

        let mut item = Dictionary::new();
        item.insert("uuid".into(), Value::String("U1".into()));
        let mut params = Dictionary::new();
        params.insert("item".into(), Value::Dictionary(item));
        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String("playlistRemove".into()));
        d.insert("params".into(), Value::Dictionary(params));
        assert_eq!(parse_action(&binary(d)).unwrap(), Action::PlaylistRemove { uuid: Some("U1".into()) });

        let mut d = Dictionary::new();
        d.insert("type".into(), Value::String("unhandledURLResponse".into()));
        assert!(parse_action(&binary(d)).is_err());
    }

    #[test]
    fn reads_query_numbers() {
        assert_eq!(query_number("/scrub?position=12.345000", "position"), Some(12.345));
        assert_eq!(query_number("/rate?value=0.000000", "value"), Some(0.0));
        assert_eq!(query_number("/rate?a=1&value=1.0", "value"), Some(1.0));
        assert_eq!(query_number("/rate", "value"), None);
        assert_eq!(query_number("/scrub?position=nan", "position"), None);
        assert_eq!(query("/setProperty?actionAtItemEnd"), "actionAtItemEnd");
    }
}
