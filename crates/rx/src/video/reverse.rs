// The reverse HTTP connection ("PTTH/1.0"): the client upgrades one of its connections with POST
// /reverse and from then on we send it requests (FCUP playlist requests and playback events).
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use plist::{Dictionary, Value};
use rustplay_core::Result;
use rustplay_core::rtsp::{ClientCodec, Headers, Protocol, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use super::Hub;

pub const XML_PLIST: &str = "text/x-apple-plist+xml";
// Values used by working receivers (apsdk, UxPlay); the client does not seem to check them.
const FCUP_CLIENT_INFO: u64 = 1;
const FCUP_CLIENT_REF: u64 = 40_030_004;
const FCUP_USER_AGENT: &str = "AppleCoreMedia/1.0.0.11B554a (Apple TV; U; CPU OS 7_0_4 like Mac OS X; en_us";

// A message the client sends on its own over the reverse connection: "POST /event EVENT/1.0"
// with a plist about the player (state changes, errors). It must be acknowledged, or the client
// gives up on the channel after about 15 seconds.
#[derive(Debug, PartialEq)]
pub struct ClientEvent {
    pub method: String,
    pub uri: String,
    pub cseq: Option<String>,
    pub body: Vec<u8>,
}

// Takes the next complete client message out of `buffer`; None while it is still incomplete or
// when it is an HTTP response (handled by the normal codec).
pub fn take_event(buffer: &mut BytesMut) -> Option<ClientEvent> {
    if buffer.starts_with(b"HTTP/") {
        return None;
    }
    let head_end = buffer.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let (method, uri) = (first.next()?.to_owned(), first.next().unwrap_or("/").to_owned());
    let (mut length, mut cseq) = (0usize, None);
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "cseq" => cseq = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    if buffer.len() < head_end + length {
        return None;
    }
    let body = buffer[head_end..head_end + length].to_vec();
    let _ = buffer.split_to(head_end + length);
    Some(ClientEvent { method, uri, cseq, body })
}

pub fn event_reply(cseq: Option<&str>) -> Vec<u8> {
    let mut reply = String::from("EVENT/1.0 200 OK\r\nContent-Length: 0\r\n");
    if let Some(cseq) = cseq {
        reply.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    reply.push_str(&format!("Server: AirTunes/{}\r\n\r\n", crate::txt::SOURCE_VERSION));
    reply.into_bytes()
}

pub fn xml(dict: Dictionary) -> Result<Bytes> {
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &Value::Dictionary(dict))?;
    Ok(Bytes::from(out))
}

fn post_event(session_id: &str, body: Bytes) -> Request {
    let mut headers = Headers::default();
    headers.set("X-Apple-Session-ID", session_id);
    headers.set("Content-Type", XML_PLIST);
    Request { method: "POST".into(), uri: "/event".into(), protocol: Protocol::Http11, headers, body }
}

// Asks the client for the playlist at `url`; it answers with POST /action on its main connection.
pub fn fcup_request(url: &str, request_id: u64, session_id: &str) -> Result<Request> {
    let mut headers = Dictionary::new();
    headers.insert("X-Playback-Session-Id".into(), Value::String(session_id.into()));
    headers.insert("User-Agent".into(), Value::String(FCUP_USER_AGENT.into()));
    let mut request = Dictionary::new();
    request.insert("FCUP_Response_ClientInfo".into(), Value::from(FCUP_CLIENT_INFO));
    request.insert("FCUP_Response_ClientRef".into(), Value::from(FCUP_CLIENT_REF));
    request.insert("FCUP_Response_RequestID".into(), Value::from(request_id));
    request.insert("FCUP_Response_URL".into(), Value::String(url.into()));
    request.insert("sessionID".into(), Value::from(1u64));
    request.insert("FCUP_Response_Headers".into(), Value::Dictionary(headers));
    let mut root = Dictionary::new();
    root.insert("sessionID".into(), Value::from(1u64));
    root.insert("type".into(), Value::String("unhandledURLRequest".into()));
    root.insert("request".into(), Value::Dictionary(request));
    Ok(post_event(session_id, xml(root)?))
}

// Tells the client the player is now "playing", "paused", "loading" or "stopped".
pub fn state_event(state: &str, session_id: &str) -> Result<Request> {
    let mut root = Dictionary::new();
    root.insert("category".into(), Value::String("video".into()));
    root.insert("sessionID".into(), Value::from(1u64));
    root.insert("state".into(), Value::String(state.into()));
    Ok(post_event(session_id, xml(root)?))
}

// Drives an upgraded connection until the client or the receiver closes it. `inbound` holds
// whatever the client already sent after its POST /reverse.
pub async fn serve(
    stream: &mut TcpStream,
    mut inbound: BytesMut,
    hub: Arc<Hub>,
    session_id: String,
    peer: SocketAddr,
    cancel: &CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
    let id = hub.add_link(&session_id, tx);
    tracing::info!(%peer, "canal inverso de AirPlay abierto (sesión {session_id})");
    let mut codec = ClientCodec;
    let mut outbound = BytesMut::with_capacity(4 * 1024);
    let result = async {
        loop {
            loop {
                if let Some(event) = take_event(&mut inbound) {
                    let text = String::from_utf8_lossy(&event.body);
                    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
                    tracing::info!(%peer, "evento del dispositivo: {} {} {text}", event.method, event.uri);
                    stream.write_all(&event_reply(event.cseq.as_deref())).await?;
                    continue;
                }
                match codec.decode(&mut inbound) {
                    Ok(Some(response)) => {
                        tracing::debug!(%peer, status = response.status, "respuesta del cliente por el canal inverso");
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // Not worth dropping the channel for: skip what could not be read.
                        tracing::warn!(%peer, "dato inesperado en el canal inverso ({e}): se descarta");
                        inbound.clear();
                        break;
                    }
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                request = rx.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    outbound.clear();
                    codec.encode(request, &mut outbound)?;
                    stream.write_all(&outbound).await?;
                }
                read = stream.read_buf(&mut inbound) => {
                    let read = read?;
                    if read == 0 {
                        tracing::info!(%peer, "el dispositivo ha cerrado el canal inverso");
                        return Ok(());
                    }
                    tracing::info!(%peer, "{read} bytes del dispositivo por el canal inverso");
                }
            }
        }
    }
    .await;
    hub.remove_link(id);
    if let Err(e) = &result {
        tracing::warn!(%peer, "el canal inverso se cierra por un error: {e}");
    }
    tracing::info!(%peer, "canal inverso de AirPlay cerrado");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_acknowledges_client_events() {
        let mut buffer = BytesMut::from(
            &b"POST /event EVENT/1.0\r\nContent-Type: text/x-apple-plist+xml\r\nContent-Length: 5\r\nCSeq: 7\r\n\r\nhelloPOST /event EVENT/1.0\r\nContent-Length: 3\r\n\r\nab"[..],
        );
        let first = take_event(&mut buffer).unwrap();
        assert_eq!((first.method.as_str(), first.uri.as_str(), first.cseq.as_deref()), ("POST", "/event", Some("7")));
        assert_eq!(first.body, b"hello");
        // The second message is still missing a byte.
        assert!(take_event(&mut buffer).is_none());
        buffer.extend_from_slice(b"c");
        assert_eq!(take_event(&mut buffer).unwrap().body, b"abc");
        assert!(buffer.is_empty());
        // Responses to our own requests are left to the normal decoder.
        let mut response = BytesMut::from(&b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"[..]);
        assert!(take_event(&mut response).is_none());
        let reply = String::from_utf8(event_reply(Some("7"))).unwrap();
        assert!(reply.starts_with("EVENT/1.0 200 OK\r\n") && reply.contains("CSeq: 7\r\n") && reply.ends_with("\r\n\r\n"));
    }

    #[test]
    fn builds_fcup_requests() {
        let request = fcup_request("mlhls://localhost/master.m3u8", 7, "SESSION").unwrap();
        assert_eq!((request.method.as_str(), request.uri.as_str()), ("POST", "/event"));
        assert_eq!(request.headers.get("X-Apple-Session-ID"), Some("SESSION"));
        assert_eq!(request.headers.get("Content-Type"), Some(XML_PLIST));
        let root = Value::from_reader_xml(std::io::Cursor::new(&request.body[..])).unwrap().into_dictionary().unwrap();
        assert_eq!(root.get("type").and_then(Value::as_string), Some("unhandledURLRequest"));
        let inner = root.get("request").and_then(Value::as_dictionary).unwrap();
        assert_eq!(inner.get("FCUP_Response_URL").and_then(Value::as_string), Some("mlhls://localhost/master.m3u8"));
        assert_eq!(inner.get("FCUP_Response_RequestID").and_then(Value::as_unsigned_integer), Some(7));
        let headers = inner.get("FCUP_Response_Headers").and_then(Value::as_dictionary).unwrap();
        assert_eq!(headers.get("X-Playback-Session-Id").and_then(Value::as_string), Some("SESSION"));
    }

    #[test]
    fn builds_state_events() {
        let request = state_event("paused", "S").unwrap();
        let root = Value::from_reader_xml(std::io::Cursor::new(&request.body[..])).unwrap().into_dictionary().unwrap();
        assert_eq!(root.get("category").and_then(Value::as_string), Some("video"));
        assert_eq!(root.get("state").and_then(Value::as_string), Some("paused"));
    }
}
