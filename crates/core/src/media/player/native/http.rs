// Blocking HTTP(S) GET with WinHTTP (part of Windows, nothing to install).

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

const MAX_BODY: usize = 96 * 1024 * 1024;
const AGENT: &str = "RustPlay (AirPlay video)";

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle de WinHTTP abierto por este módulo y cerrado una sola vez.
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn open(handle: *mut c_void, what: &str) -> Result<Handle, String> {
    if handle.is_null() { Err(format!("{what}: {}", windows::core::Error::from_thread())) } else { Ok(Handle(handle)) }
}

// (secure, host, port, path with query)
pub(super) fn split(url: &str) -> Result<(bool, String, u16, String), String> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("dirección no admitida: {url}"));
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_owned()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            (host, port.parse::<u16>().map_err(|_| "puerto no válido".to_owned())?)
        }
        _ => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("dirección sin servidor".into());
    }
    Ok((secure, host.to_owned(), port, path))
}

// Downloads `url`; any status other than 2xx is an error.
pub(super) fn get(url: &str) -> Result<Vec<u8>, String> {
    let (secure, host, port, path) = split(url)?;
    // SAFETY: llamadas a WinHTTP con manejadores propios y búferes válidos durante cada llamada.
    unsafe {
        let session = open(
            WinHttpOpen(&HSTRING::from(AGENT), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0),
            "WinHttpOpen",
        )?;
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
        let connect = open(WinHttpConnect(session.0, &HSTRING::from(host), port, 0), "WinHttpConnect")?;
        let request = open(
            WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
            ),
            "WinHttpOpenRequest",
        )?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| format!("WinHttpSendRequest: {e}"))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| format!("WinHttpReceiveResponse: {e}"))?;
        let mut status = 0u32;
        let mut len = size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("WinHttpQueryHeaders: {e}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read)
                .map_err(|e| format!("WinHttpReadData: {e}"))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_BODY {
                return Err("respuesta demasiado grande".into());
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_urls() {
        assert_eq!(split("https://a.b/c/d?x=1").unwrap(), (true, "a.b".into(), 443, "/c/d?x=1".into()));
        assert_eq!(split("http://127.0.0.1:7000/master.m3u8").unwrap(), (false, "127.0.0.1".into(), 7000, "/master.m3u8".into()));
        assert_eq!(split("http://h?q=1").unwrap(), (false, "h".into(), 80, "/?q=1".into()));
        assert!(split("ftp://x").is_err());
        assert!(split("http:///x").is_err());
    }

    // A local server answers; WinHTTP reaches it over loopback (no firewall prompt).
    #[test]
    fn downloads_from_a_local_server() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (status, body) = if request.starts_with("GET /ok ") { ("200 OK", "hola") } else { ("404 Not Found", "") };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        assert_eq!(super::get(&format!("http://127.0.0.1:{port}/ok")).unwrap(), b"hola");
        assert_eq!(super::get(&format!("http://127.0.0.1:{port}/missing")).unwrap_err(), "HTTP 404");
        server.join().unwrap();
    }
}
