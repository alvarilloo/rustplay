use std::net::SocketAddr;
use std::time::Duration;

use rustplay_core::crypto::session::HapCodec;
use rustplay_core::rtsp::{ClientCodec, Headers, Protocol, Request, Response};
use rustplay_core::{Error, Result};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::codec::{Decoder, Encoder};

pub const USER_AGENT: &str = "AirPlay/935.7.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn client_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|n| n.is_ascii() && !n.is_empty())
        .unwrap_or_else(|| String::from("PC con Windows"))
}

pub struct RtspConn {
    stream: TcpStream,
    codec: HapCodec<ClientCodec>,
    read_buf: BytesMut,
    write_buf: BytesMut,
    cseq: u32,
    dacp_id: String,
    active_remote: String,
    minimal: bool,
    pub peer: SocketAddr,
}

impl RtspConn {
    pub async fn connect(peer: SocketAddr) -> Result<Self> {
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(peer))
            .await
            .map_err(|_| Error::Timeout)??;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            codec: HapCodec::new(ClientCodec),
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(4096),
            cseq: 0,
            dacp_id: format!("{:016X}", rand::random::<u64>()),
            active_remote: rand::random::<u32>().to_string(),
            minimal: false,
            peer,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.stream.local_addr()?)
    }

    pub fn enable_encryption(&mut self, read_key: [u8; 32], write_key: [u8; 32]) {
        self.codec.enable(read_key, write_key);
    }

    // doubletake sends only CSeq and User-Agent after pairing.
    pub fn set_minimal_headers(&mut self, minimal: bool) {
        self.minimal = minimal;
    }

    pub fn is_encrypted(&self) -> bool {
        self.codec.enabled
    }

    pub async fn request(
        &mut self,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Response> {
        self.cseq += 1;
        let mut headers = Headers::default();
        headers.set("CSeq", self.cseq.to_string());
        headers.set("User-Agent", USER_AGENT);
        if !self.minimal {
            headers.set("X-Apple-Client-Name", client_name());
            headers.set("Client-Instance", self.dacp_id.clone());
            headers.set("DACP-ID", self.dacp_id.clone());
            headers.set("Active-Remote", self.active_remote.clone());
        }
        for (name, value) in extra {
            headers.set(*name, *value);
        }
        if let Some(content_type) = content_type.filter(|_| !body.is_empty()) {
            headers.set("Content-Type", content_type);
        }
        let request = Request {
            method: method.to_owned(),
            uri: uri.to_owned(),
            protocol: Protocol::Rtsp10,
            headers,
            body: Bytes::copy_from_slice(body),
        };
        self.write_buf.clear();
        self.codec.encode(request, &mut self.write_buf)?;
        self.stream.write_all(&self.write_buf).await?;
        timeout(REQUEST_TIMEOUT, self.read_response())
            .await
            .map_err(|_| Error::Timeout)?
    }

    async fn read_response(&mut self) -> Result<Response> {
        loop {
            if let Some(response) = self.codec.decode(&mut self.read_buf)? {
                return Ok(response);
            }
            if self.stream.read_buf(&mut self.read_buf).await? == 0 {
                return Err(Error::protocol("el receptor cerró la conexión"));
            }
        }
    }

    // Like `request` but any status other than 200 becomes a protocol error naming the step.
    pub async fn call(
        &mut self,
        what: &str,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Response> {
        let response = self.request(method, uri, extra, content_type, body).await?;
        ensure_ok(what, &response)?;
        Ok(response)
    }
}

pub fn ensure_ok(what: &str, response: &Response) -> Result<()> {
    if response.status == 200 {
        return Ok(());
    }
    let hint = match response.status {
        401 => " (el receptor pide contraseña; no está soportada todavía)",
        403 => " (acceso denegado por el receptor)",
        453 => " (el receptor está ocupado)",
        470 => " (se requiere PIN o emparejamiento)",
        _ => "",
    };
    Err(Error::Protocol(format!(
        "{what}: el receptor respondió {}{hint}",
        response.status
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ok_status_is_an_error_with_hint() {
        let response = Response::new(Protocol::Rtsp10, 470, "");
        let error = ensure_ok("SETUP", &response).unwrap_err().to_string();
        assert!(error.contains("470") && error.contains("PIN"));
        assert!(ensure_ok("SETUP", &Response::ok(Protocol::Rtsp10)).is_ok());
    }
}
