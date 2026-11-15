// DACP remote control of the device that streams audio to us.
//
// While an iPhone streams audio (Music, Podcasts...), its RTSP requests carry `DACP-ID` (64-bit
// hex) and `Active-Remote` (a decimal token). The phone also announces `_dacp._tcp` as
// `iTunes_Ctrl_<DACP-ID>` on a dynamic port; an HTTP GET to `/ctrl-int/1/<command>` with the
// `Active-Remote` header there controls playback, and the phone answers `204 No Content`.
// Screen mirroring sessions do not send these headers, so this only works for audio streaming.
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustplay_core::mdns;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DACP_SERVICE: &str = "_dacp._tcp";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCommand {
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
}

impl RemoteCommand {
    pub fn path(self) -> &'static str {
        match self {
            RemoteCommand::PlayPause => "/ctrl-int/1/playpause",
            RemoteCommand::Next => "/ctrl-int/1/nextitem",
            RemoteCommand::Previous => "/ctrl-int/1/previtem",
            RemoteCommand::VolumeUp => "/ctrl-int/1/volumeup",
            RemoteCommand::VolumeDown => "/ctrl-int/1/volumedown",
        }
    }
}

pub fn instance_name(dacp_id: &str) -> String {
    format!("iTunes_Ctrl_{dacp_id}")
}

pub fn build_request(command: RemoteCommand, host: &str, active_remote: &str) -> String {
    format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nActive-Remote: {active_remote}\r\nConnection: close\r\n\r\n",
        command.path()
    )
}

// Status code of an HTTP response's first line, e.g. 204 for "HTTP/1.1 204 No Content".
pub fn parse_status(response: &[u8]) -> Option<u16> {
    let line = response.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    parts.next().filter(|p| p.starts_with("HTTP/"))?;
    parts.next()?.parse().ok()
}

// Sends one command and returns the HTTP status.
pub async fn send_command(addr: SocketAddr, command: RemoteCommand, active_remote: &str) -> std::io::Result<u16> {
    let exchange = async {
        let mut stream = TcpStream::connect(addr).await?;
        let host = match addr.ip() {
            IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
            IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
        };
        stream.write_all(build_request(command, &host, active_remote).as_bytes()).await?;
        let mut buf = [0u8; 512];
        let mut len = 0;
        while len < buf.len() {
            let n = stream.read(&mut buf[len..]).await?;
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].contains(&b'\n') {
                break;
            }
        }
        parse_status(&buf[..len]).ok_or_else(|| std::io::Error::other("respuesta DACP no válida"))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, exchange)
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

// Looks up the port of the phone's DACP service.
async fn resolve_port(dacp_id: &str) -> Option<u16> {
    let wanted = instance_name(dacp_id);
    let mut browser = match mdns::browse(DACP_SERVICE) {
        Ok(browser) => browser,
        Err(e) => {
            tracing::warn!("no se pudo buscar el control remoto DACP: {e}");
            return None;
        }
    };
    let found = tokio::time::timeout(RESOLVE_TIMEOUT, async {
        while let Some(device) = browser.next().await {
            if device.instance.eq_ignore_ascii_case(&wanted) {
                return Some(device.port);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    drop(browser);
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub peer: SocketAddr,
    pub dacp_id: String,
    pub active_remote: String,
    port: Option<u16>,
}

// The device that can currently be controlled (the last one that streamed audio with DACP).
#[derive(Default)]
pub struct Remotes {
    current: Mutex<Option<Target>>,
}

impl Remotes {
    pub fn offer(&self, peer: SocketAddr, dacp_id: &str, active_remote: &str) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        let same = current
            .as_ref()
            .is_some_and(|t| t.peer == peer && t.dacp_id == dacp_id && t.active_remote == active_remote);
        if !same {
            tracing::info!(%peer, dacp_id, "el dispositivo permite control remoto (DACP)");
            *current = Some(Target {
                peer,
                dacp_id: dacp_id.to_owned(),
                active_remote: active_remote.to_owned(),
                port: None,
            });
        }
    }

    pub fn withdraw(&self, peer: SocketAddr) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_ref().is_some_and(|t| t.peer == peer) {
            *current = None;
        }
    }

    fn target(&self) -> Option<Target> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn remember_port(&self, target: &Target, port: Option<u16>) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(t) = current.as_mut().filter(|t| t.peer == target.peer && t.dacp_id == target.dacp_id) {
            t.port = port;
        }
    }

    pub fn send(self: &Arc<Self>, command: RemoteCommand) -> bool {
        let Some(target) = self.target() else {
            tracing::info!("no hay ningún dispositivo que admita control remoto");
            return false;
        };
        let remotes = Arc::clone(self);
        tokio::spawn(async move { remotes.run(target, command).await });
        true
    }

    async fn run(&self, target: Target, command: RemoteCommand) {
        let ip = target.peer.ip();
        for attempt in 0..2 {
            let port = match target.port.filter(|_| attempt == 0) {
                Some(port) => Some(port),
                None => resolve_port(&target.dacp_id).await,
            };
            let Some(port) = port else {
                tracing::warn!(peer = %ip, "no se encontró el servicio DACP del dispositivo");
                return;
            };
            self.remember_port(&target, Some(port));
            let addr = SocketAddr::new(ip, port);
            match send_command(addr, command, &target.active_remote).await {
                Ok(status) if (200..300).contains(&status) => {
                    tracing::debug!(%addr, ?command, status, "orden DACP enviada");
                    return;
                }
                Ok(status) => {
                    tracing::warn!(%addr, ?command, status, "el dispositivo rechazó la orden DACP");
                    return;
                }
                Err(e) => {
                    tracing::debug!(%addr, "fallo al enviar la orden DACP: {e}");
                    self.remember_port(&target, None);
                }
            }
        }
        tracing::warn!(peer = %ip, ?command, "no se pudo enviar la orden de control remoto");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn request_has_path_host_and_active_remote() {
        let request = build_request(RemoteCommand::PlayPause, "192.168.1.20:52000", "1986535575");
        assert_eq!(
            request,
            "GET /ctrl-int/1/playpause HTTP/1.1\r\nHost: 192.168.1.20:52000\r\n\
             Active-Remote: 1986535575\r\nConnection: close\r\n\r\n"
        );
        assert!(build_request(RemoteCommand::Next, "h", "1").starts_with("GET /ctrl-int/1/nextitem "));
        assert!(build_request(RemoteCommand::Previous, "h", "1").starts_with("GET /ctrl-int/1/previtem "));
        assert!(build_request(RemoteCommand::VolumeUp, "h", "1").starts_with("GET /ctrl-int/1/volumeup "));
        assert!(build_request(RemoteCommand::VolumeDown, "h", "1").starts_with("GET /ctrl-int/1/volumedown "));
    }

    #[test]
    fn instance_name_matches_itunes_convention() {
        assert_eq!(instance_name("A1B2C3D4E5F60718"), "iTunes_Ctrl_A1B2C3D4E5F60718");
    }

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status(b"HTTP/1.1 204 No Content\r\n\r\n"), Some(204));
        assert_eq!(parse_status(b"HTTP/1.0 403 Forbidden\r\n"), Some(403));
        assert_eq!(parse_status(b"RTSP/1.0 200 OK\r\n"), None);
        assert_eq!(parse_status(b""), None);
    }

    #[test]
    fn remotes_keep_only_the_offering_peer() {
        let remotes = Remotes::default();
        let a: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        remotes.offer(a, "ID", "42");
        remotes.withdraw(b);
        assert_eq!(remotes.target().map(|t| t.active_remote), Some("42".to_owned()));
        remotes.withdraw(a);
        assert!(remotes.target().is_none());
    }

    #[tokio::test]
    async fn sends_the_request_to_a_loopback_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let mut len = 0;
            while !buf[..len].ends_with(b"\r\n\r\n") {
                len += stream.read(&mut buf[len..]).await.unwrap();
            }
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await.unwrap();
            String::from_utf8(buf[..len].to_vec()).unwrap()
        });
        let status = send_command(addr, RemoteCommand::Next, "77").await.unwrap();
        assert_eq!(status, 204);
        let request = server.await.unwrap();
        assert!(request.starts_with("GET /ctrl-int/1/nextitem HTTP/1.1\r\n"));
        assert!(request.contains("\r\nActive-Remote: 77\r\n"));
    }
}
