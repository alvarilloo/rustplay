use std::fmt::Write as _;

use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Rtsp10,
    Http11,
}

#[derive(Debug, Default, Clone)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        match self
            .0
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        {
            Some(entry) => entry.1 = value,
            None => self.0.push((name, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub protocol: Protocol,
    pub headers: Headers,
    pub body: Bytes,
}

impl Request {
    pub fn cseq(&self) -> Option<&str> {
        self.headers.get("CSeq")
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub protocol: Protocol,
    pub status: u16,
    pub reason: &'static str,
    pub headers: Headers,
    pub body: Bytes,
}

impl Response {
    pub fn new(protocol: Protocol, status: u16, reason: &'static str) -> Self {
        Self {
            protocol,
            status,
            reason,
            headers: Headers::default(),
            body: Bytes::new(),
        }
    }

    pub fn ok(protocol: Protocol) -> Self {
        Self::new(protocol, 200, "OK")
    }

    pub fn with_body(mut self, content_type: &str, body: Bytes) -> Self {
        self.headers.set("Content-Type", content_type);
        self.body = body;
        self
    }
}

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct ServerCodec;

#[derive(Debug, Default)]
pub struct ClientCodec;

struct Head {
    head_len: usize,
    total_len: usize,
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn skip_blank_lines(src: &mut BytesMut) {
    let n = src
        .iter()
        .take_while(|b| matches!(**b, b'\r' | b'\n'))
        .count();
    if n > 0 {
        let _ = src.split_to(n);
    }
}

fn parse_head(src: &[u8]) -> Result<Option<(Head, &str, Headers)>> {
    let Some(end) = find_head_end(src) else {
        if src.len() > MAX_HEAD {
            return Err(Error::protocol("cabeceras demasiado largas"));
        }
        return Ok(None);
    };
    let head_len = end + 4;
    if head_len > MAX_HEAD {
        return Err(Error::protocol("cabeceras demasiado largas"));
    }
    let text =
        std::str::from_utf8(&src[..end]).map_err(|_| Error::protocol("cabeceras no son UTF-8"))?;
    let mut lines = text.split("\r\n");
    let start = lines.next().unwrap_or_default();
    let mut headers = Headers::default();
    let mut content_length = 0usize;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::protocol("línea de cabecera no válida"))?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(Error::protocol("nombre de cabecera vacío"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse()
                .map_err(|_| Error::protocol("Content-Length no válido"))?;
        }
        headers.0.push((name.to_owned(), value.to_owned()));
    }
    if content_length > MAX_BODY {
        return Err(Error::protocol("cuerpo demasiado grande"));
    }
    Ok(Some((
        Head {
            head_len,
            total_len: head_len + content_length,
        },
        start,
        headers,
    )))
}

fn protocol_from(token: &str) -> Result<Protocol> {
    match token {
        "RTSP/1.0" => Ok(Protocol::Rtsp10),
        "HTTP/1.1" | "HTTP/1.0" => Ok(Protocol::Http11),
        _ => Err(Error::protocol(format!("protocolo no admitido: {token}"))),
    }
}

fn protocol_str(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Rtsp10 => "RTSP/1.0",
        Protocol::Http11 => "HTTP/1.1",
    }
}

fn reason_for(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        453 => "Not Enough Bandwidth",
        454 => "Session Not Found",
        455 => "Method Not Valid in This State",
        459 => "Aggregate Operation Not Allowed",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "",
    }
}

fn write_headers(
    headers: &Headers,
    body_len: usize,
    force_length: bool,
    dst: &mut BytesMut,
) -> Result<()> {
    for (name, value) in headers.iter() {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            return Err(Error::protocol("cabecera con caracteres no válidos"));
        }
        dst.reserve(name.len() + value.len() + 4);
        dst.extend_from_slice(name.as_bytes());
        dst.extend_from_slice(b": ");
        dst.extend_from_slice(value.as_bytes());
        dst.extend_from_slice(b"\r\n");
    }
    if body_len > 0 || force_length {
        let _ = write!(dst, "Content-Length: {body_len}\r\n");
    }
    dst.extend_from_slice(b"\r\n");
    Ok(())
}

impl Decoder for ServerCodec {
    type Item = Request;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Request>> {
        skip_blank_lines(src);
        let Some((head, start, headers)) = parse_head(src)? else {
            return Ok(None);
        };
        if src.len() < head.total_len {
            src.reserve(head.total_len - src.len());
            return Ok(None);
        }
        let mut parts = start.split(' ').filter(|p| !p.is_empty());
        let (Some(method), Some(uri), Some(proto), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::protocol("línea de petición no válida"));
        };
        let protocol = protocol_from(proto)?;
        let method = method.to_owned();
        let uri = uri.to_owned();
        let _ = src.split_to(head.head_len);
        let body = src.split_to(head.total_len - head.head_len).freeze();
        Ok(Some(Request {
            method,
            uri,
            protocol,
            headers,
            body,
        }))
    }
}

impl Encoder<Response> for ServerCodec {
    type Error = Error;

    fn encode(&mut self, item: Response, dst: &mut BytesMut) -> Result<()> {
        dst.reserve(64 + item.body.len());
        let _ = write!(
            dst,
            "{} {} {}\r\n",
            protocol_str(item.protocol),
            item.status,
            item.reason
        );
        write_headers(&item.headers, item.body.len(), true, dst)?;
        dst.extend_from_slice(&item.body);
        Ok(())
    }
}

impl Decoder for ClientCodec {
    type Item = Response;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Response>> {
        skip_blank_lines(src);
        let Some((head, start, headers)) = parse_head(src)? else {
            return Ok(None);
        };
        if src.len() < head.total_len {
            src.reserve(head.total_len - src.len());
            return Ok(None);
        }
        let mut parts = start.splitn(3, ' ');
        let protocol = protocol_from(parts.next().unwrap_or_default())?;
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::protocol("línea de estado no válida"))?;
        let _ = src.split_to(head.head_len);
        let body = src.split_to(head.total_len - head.head_len).freeze();
        Ok(Some(Response {
            protocol,
            status,
            reason: reason_for(status),
            headers,
            body,
        }))
    }
}

impl Encoder<Request> for ClientCodec {
    type Error = Error;

    fn encode(&mut self, item: Request, dst: &mut BytesMut) -> Result<()> {
        if item.method.contains([' ', '\r', '\n']) || item.uri.contains([' ', '\r', '\n']) {
            return Err(Error::protocol("método o URI no válidos"));
        }
        dst.reserve(64 + item.uri.len() + item.body.len());
        let _ = write!(
            dst,
            "{} {} {}\r\n",
            item.method,
            item.uri,
            protocol_str(item.protocol)
        );
        write_headers(&item.headers, item.body.len(), false, dst)?;
        dst.extend_from_slice(&item.body);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, uri: &str, protocol: Protocol) -> Request {
        Request {
            method: method.into(),
            uri: uri.into(),
            protocol,
            headers: Headers::default(),
            body: Bytes::new(),
        }
    }

    #[test]
    fn decodes_rtsp_request_with_case_insensitive_headers() {
        let mut buf =
            BytesMut::from(&b"GET /info RTSP/1.0\r\ncseq: 7\r\nUser-Agent: x\r\n\r\n"[..]);
        let r = ServerCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.uri, "/info");
        assert_eq!(r.protocol, Protocol::Rtsp10);
        assert_eq!(r.cseq(), Some("7"));
        assert_eq!(r.headers.get("USER-AGENT"), Some("x"));
        assert!(r.body.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn decodes_http_request_with_body() {
        let mut buf =
            BytesMut::from(&b"POST /pair-setup HTTP/1.1\r\nContent-Length: 4\r\n\r\nabcd"[..]);
        let r = ServerCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(r.protocol, Protocol::Http11);
        assert_eq!(&r.body[..], b"abcd");
    }

    #[test]
    fn partial_input_returns_none_until_complete() {
        let full = b"POST /x RTSP/1.0\r\nCSeq: 1\r\ncontent-length: 5\r\n\r\nhello";
        let mut codec = ServerCodec;
        let mut buf = BytesMut::new();
        for (i, b) in full.iter().enumerate() {
            buf.extend_from_slice(&[*b]);
            let out = codec.decode(&mut buf).unwrap();
            if i + 1 < full.len() {
                assert!(out.is_none(), "completed early at byte {i}");
            } else {
                assert_eq!(&out.unwrap().body[..], b"hello");
            }
        }
    }

    #[test]
    fn pipelined_requests() {
        let mut buf = BytesMut::from(
            &b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nPOST /a RTSP/1.0\r\nCSeq: 2\r\nContent-Length: 2\r\n\r\nhiGET /info RTSP/1.0\r\nCSeq: 3\r\n\r\n"[..],
        );
        let mut codec = ServerCodec;
        let a = codec.decode(&mut buf).unwrap().unwrap();
        let b = codec.decode(&mut buf).unwrap().unwrap();
        let c = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!((a.method.as_str(), a.cseq()), ("OPTIONS", Some("1")));
        assert_eq!((b.method.as_str(), &b.body[..]), ("POST", &b"hi"[..]));
        assert_eq!((c.uri.as_str(), c.cseq()), ("/info", Some("3")));
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn rejects_oversized_headers_and_body() {
        let mut buf = BytesMut::from(&b"GET / RTSP/1.0\r\n"[..]);
        buf.extend_from_slice(&vec![b'a'; MAX_HEAD + 10]);
        assert!(matches!(
            ServerCodec.decode(&mut buf),
            Err(Error::Protocol(_))
        ));

        let mut buf = BytesMut::from(&b"POST / RTSP/1.0\r\nContent-Length: 16777217\r\n\r\n"[..]);
        assert!(matches!(
            ServerCodec.decode(&mut buf),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn rejects_malformed_lines() {
        let mut buf = BytesMut::from(&b"GARBAGE\r\n\r\n"[..]);
        assert!(ServerCodec.decode(&mut buf).is_err());
        let mut buf = BytesMut::from(&b"GET / FTP/1.0\r\n\r\n"[..]);
        assert!(ServerCodec.decode(&mut buf).is_err());
        let mut buf = BytesMut::from(&b"GET / RTSP/1.0\r\nnocolon\r\n\r\n"[..]);
        assert!(ServerCodec.decode(&mut buf).is_err());
    }

    #[test]
    fn encodes_response_preserving_order_and_adding_length() {
        let mut resp = Response::ok(Protocol::Rtsp10);
        resp.headers.set("CSeq", "3");
        resp.headers.set("Server", "AirTunes/220.68");
        let mut out = BytesMut::new();
        ServerCodec.encode(resp.clone(), &mut out).unwrap();
        assert_eq!(
            &out[..],
            b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nServer: AirTunes/220.68\r\nContent-Length: 0\r\n\r\n"
        );

        let resp = resp.with_body("application/octet-stream", Bytes::from_static(b"xyz"));
        let mut out = BytesMut::new();
        ServerCodec.encode(resp, &mut out).unwrap();
        assert!(out.ends_with(b"Content-Length: 3\r\n\r\nxyz"));
    }

    #[test]
    fn client_roundtrip_over_server_codec() {
        let mut r = req("POST", "/fp-setup", Protocol::Rtsp10);
        r.headers.set("CSeq", "9");
        r.body = Bytes::from_static(b"payload");
        let mut wire = BytesMut::new();
        ClientCodec.encode(r, &mut wire).unwrap();
        let got = ServerCodec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(got.uri, "/fp-setup");
        assert_eq!(got.headers.get("content-length"), Some("7"));
        assert_eq!(&got.body[..], b"payload");

        let mut resp = Response::ok(Protocol::Http11);
        resp.headers.set("CSeq", "9");
        let mut wire = BytesMut::new();
        ServerCodec
            .encode(
                resp.with_body("text/plain", Bytes::from_static(b"ok")),
                &mut wire,
            )
            .unwrap();
        let got = ClientCodec.decode(&mut wire).unwrap().unwrap();
        assert_eq!(
            (got.status, got.reason, got.protocol),
            (200, "OK", Protocol::Http11)
        );
        assert_eq!(&got.body[..], b"ok");
    }

    #[test]
    fn client_request_without_body_omits_content_length() {
        let mut out = BytesMut::new();
        ClientCodec
            .encode(req("OPTIONS", "*", Protocol::Rtsp10), &mut out)
            .unwrap();
        assert_eq!(&out[..], b"OPTIONS * RTSP/1.0\r\n\r\n");
    }

    #[test]
    fn client_decodes_partial_and_status_without_reason() {
        let mut buf = BytesMut::from(&b"RTSP/1.0 501\r\nCSeq: 1\r\n"[..]);
        assert!(ClientCodec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"\r\n");
        let got = ClientCodec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(got.status, 501);
    }

    #[test]
    fn rejects_header_injection_on_encode() {
        let mut resp = Response::ok(Protocol::Rtsp10);
        resp.headers.set("X", "a\r\nEvil: 1");
        assert!(ServerCodec.encode(resp, &mut BytesMut::new()).is_err());
    }
}
