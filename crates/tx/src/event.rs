use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rustplay_core::crypto::session::HapCodec;
use rustplay_core::crypto::stream::event_keys;
use rustplay_core::rtsp::{Response, ServerCodec};
use rustplay_core::{Error, Result};
use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_util::codec::{Decoder, Encoder};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

// The receiver sends RTSP requests towards us on its eventPort; each one only needs a 200.
pub struct EventChannel {
    task: JoinHandle<()>,
    received: Arc<AtomicU64>,
}

impl EventChannel {
    // `secret` is the key material of the HAP session when the control channel is encrypted.
    pub async fn connect(peer: SocketAddr, secret: Option<&[u8]>) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(peer))
            .await
            .map_err(|_| Error::Timeout)??;
        stream.set_nodelay(true)?;
        let mut codec = HapCodec::new(ServerCodec);
        if let Some(secret) = secret {
            let (read_key, write_key) = event_keys(secret)?;
            codec.enable(read_key, write_key);
        }
        tracing::debug!(%peer, cifrado = secret.is_some(), "canal de eventos conectado");
        let received = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(serve(stream, codec, Arc::clone(&received)));
        Ok(Self { task, received })
    }

    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

impl Drop for EventChannel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve<S>(mut stream: S, mut codec: HapCodec<ServerCodec>, received: Arc<AtomicU64>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut inbound = BytesMut::with_capacity(4096);
    let mut outbound = BytesMut::with_capacity(1024);
    loop {
        loop {
            let request = match codec.decode(&mut inbound) {
                Ok(Some(request)) => request,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!("canal de eventos: {e}");
                    return;
                }
            };
            received.fetch_add(1, Ordering::Relaxed);
            let body = plist::Value::from_reader(std::io::Cursor::new(&request.body[..]))
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            tracing::debug!(
                "evento del receptor: {} {} ({} bytes) {body}",
                request.method,
                request.uri,
                request.body.len()
            );
            let mut response = Response::ok(request.protocol);
            if let Some(cseq) = request.cseq() {
                response.headers.set("CSeq", cseq);
            }
            outbound.clear();
            if codec.encode(response, &mut outbound).is_err() || stream.write_all(&outbound).await.is_err() {
                return;
            }
        }
        match stream.read_buf(&mut inbound).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use rustplay_core::rtsp::{ClientCodec, Headers, Protocol, Request};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn answers_pipelined_plaintext_requests_with_matching_cseq() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let channel = EventChannel::connect(listener.local_addr().unwrap(), None)
            .await
            .unwrap();
        let (mut receiver, _) = listener.accept().await.unwrap();
        receiver
            .write_all(
                b"POST /command RTSP/1.0\r\nCSeq: 5\r\nContent-Length: 3\r\n\r\nabcPOST /command RTSP/1.0\r\nCSeq: 6\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 512];
        while seen.windows(4).filter(|w| *w == b"\r\n\r\n").count() < 2 {
            let n = tokio::time::timeout(Duration::from_secs(2), receiver.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0);
            seen.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8(seen).unwrap();
        let first = text.find("CSeq: 5").expect("falta CSeq 5");
        let second = text.find("CSeq: 6").expect("falta CSeq 6");
        assert!(first < second);
        assert_eq!(text.matches("RTSP/1.0 200 OK").count(), 2);
        assert_eq!(channel.received(), 2);
    }

    #[tokio::test]
    async fn encrypted_channel_roundtrips_with_the_receiver_side_codec() {
        let secret = [3u8; 64];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _channel = EventChannel::connect(listener.local_addr().unwrap(), Some(&secret))
            .await
            .unwrap();
        let (mut receiver, _) = listener.accept().await.unwrap();
        // The accessory writes with the keys our channel reads with and reads with the other pair.
        let (read_key, write_key) = event_keys(&secret).unwrap();
        let mut codec = HapCodec::new(ClientCodec);
        codec.enable(write_key, read_key);
        let mut headers = Headers::default();
        headers.set("CSeq", "9");
        let request = Request {
            method: "POST".into(),
            uri: "/command".into(),
            protocol: Protocol::Rtsp10,
            headers,
            body: bytes::Bytes::new(),
        };
        let mut wire = BytesMut::new();
        codec.encode(request, &mut wire).unwrap();
        receiver.write_all(&wire).await.unwrap();
        let mut inbound = BytesMut::new();
        let response = loop {
            if let Some(r) = codec.decode(&mut inbound).unwrap() {
                break r;
            }
            let n = tokio::time::timeout(Duration::from_secs(2), receiver.read_buf(&mut inbound))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0);
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.headers.get("CSeq"), Some("9"));
    }
}
