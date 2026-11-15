// Self-update: downloads the installer of a newer release over HTTPS (WinHTTP), checks its SHA-256
// against a checksum asset of the same release and hands it to the user for installation.
//
// Safety rules: only release assets of github.com (and the two hosts it redirects to) are fetched;
// redirects are followed by hand so every hop is checked; a release without a checksum asset is
// never run; nothing is launched without the user's explicit confirmation (see ui/extras.rs).
// MSIX (Store) and portable installs never come here: they only link to the release page.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, SW_SHOWNORMAL, WM_APP};
use windows::core::{HSTRING, PCWSTR};

use crate::update::Asset;

// Asks the main window to exit once the installer has been started.
pub const WM_QUIT_FOR_UPDATE: u32 = WM_APP + 30;

// Inno Setup flags; keep them consistent with packaging/inno/RustPlay.iss (CloseApplications=yes).
const INNO_SILENT: &str = "/SILENT /CLOSEAPPLICATIONS /RESTARTAPPLICATIONS";
const INNO_NORMAL: &str = "/CLOSEAPPLICATIONS /RESTARTAPPLICATIONS";

const ALLOWED_HOSTS: [&str; 3] = ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
const MAX_INSTALLER: u64 = 512 << 20;
const MAX_CHECKSUMS: u64 = 64 << 10;
const MAX_REDIRECTS: usize = 6;

// ---------------------------------------------------------------- pure logic

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstallerKind {
    Exe,
    Msi,
    Msix,
}

fn installer_kind(name: &str) -> Option<InstallerKind> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("rustplay-setup-") && lower.ends_with(".exe") {
        Some(InstallerKind::Exe)
    } else if lower.starts_with("rustplay") && lower.ends_with(".msi") {
        Some(InstallerKind::Msi)
    } else if lower.starts_with("rustplay") && (lower.ends_with(".msix") || lower.ends_with(".msixbundle")) {
        Some(InstallerKind::Msix)
    } else {
        None
    }
}

// The installer asset of a release: the Inno Setup `RustPlay-Setup-*.exe` first, then an MSI, then
// an MSIX. Everything else (portable zip, symbols, signatures...) is ignored.
pub fn pick_installer(assets: &[Asset]) -> Option<(&Asset, InstallerKind)> {
    [InstallerKind::Exe, InstallerKind::Msi, InstallerKind::Msix]
        .into_iter()
        .find_map(|kind| assets.iter().find(|a| installer_kind(&a.name) == Some(kind)).map(|a| (a, kind)))
}

// The checksum asset for `installer`: its own `<name>.sha256`, else a `SHA256SUMS` list.
pub fn pick_checksum_asset<'a>(assets: &'a [Asset], installer: &str) -> Option<&'a Asset> {
    let own = format!("{}.sha256", installer.to_ascii_lowercase());
    assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase() == own)
        .or_else(|| assets.iter().find(|a| matches!(a.name.to_ascii_lowercase().as_str(), "sha256sums" | "sha256sums.txt")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(text: &str) -> Option<Url> {
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (secure, rest) = match (text.strip_prefix("https://"), text.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (_, Some(rest)) => (false, rest),
        _ => return None,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return None;
    }
    let tail = tail.split('#').next().unwrap_or("");
    let path = if tail.is_empty() { String::from("/") } else if tail.starts_with('?') { format!("/{tail}") } else { tail.to_owned() };
    Some(Url { secure, host: host.to_ascii_lowercase(), port, path })
}

// Download URLs are only accepted on GitHub's own hosts, over HTTPS.
pub fn is_allowed_download_url(url: &Url) -> bool {
    url.secure && url.port == 443 && ALLOWED_HOSTS.contains(&url.host.as_str())
}

#[cfg(test)]
pub fn is_allowed_download_text(text: &str) -> bool {
    parse_url(text).is_some_and(|u| is_allowed_download_url(&u))
}

// Resolves a redirect `Location` against the URL it answered.
fn resolve_location(base: &Url, location: &str) -> Option<Url> {
    if location.starts_with("http://") || location.starts_with("https://") {
        parse_url(location)
    } else if location.starts_with('/') && !location.starts_with("//") {
        parse_url(&format!("{}://{}:{}{location}", if base.secure { "https" } else { "http" }, base.host, base.port))
    } else {
        None
    }
}

// One entry per valid line of a checksum file: the digest and the file name it names, if any.
// Accepts `<hex>  name`, `<hex> *name` (sha256sum) and a bare `<hex>`.
pub fn parse_checksums(text: &str) -> Vec<([u8; 32], Option<String>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        let (hex, name) = match line.split_once(char::is_whitespace) {
            Some((hex, name)) => (hex, Some(name.trim().trim_start_matches('*').trim().to_owned())),
            None => (line, None),
        };
        if let Some(digest) = parse_hex_digest(hex) {
            out.push((digest, name.filter(|n| !n.is_empty())));
        }
    }
    out
}

fn parse_hex_digest(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

// The expected digest of `file_name`. A bare digest (no name) counts only when `dedicated` is true,
// i.e. the file is the installer's own `.sha256`.
pub fn expected_digest(text: &str, file_name: &str, dedicated: bool) -> Option<[u8; 32]> {
    let entries = parse_checksums(text);
    entries
        .iter()
        .find(|(_, name)| name.as_deref().is_some_and(|n| n.rsplit(['/', '\\']).next().is_some_and(|n| n.eq_ignore_ascii_case(file_name))))
        .or_else(|| entries.iter().find(|(_, name)| dedicated && name.is_none()))
        .map(|(digest, _)| *digest)
}

#[cfg(test)]
pub fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut chunk)?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
pub fn verify_file(path: &Path, expected: &[u8; 32]) -> std::io::Result<bool> {
    Ok(&sha256_file(path)? == expected)
}

// File names from the release come from the network: keep a safe subset.
fn safe_file_name(name: &str) -> Option<String> {
    let ok = !name.is_empty() && name.len() <= 100 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then(|| name.to_owned())
}

pub fn install_args(kind: InstallerKind, silent: bool) -> &'static str {
    match (kind, silent) {
        (InstallerKind::Exe, true) => INNO_SILENT,
        (InstallerKind::Exe, false) => INNO_NORMAL,
        _ => "",
    }
}

// ---------------------------------------------------------------- download

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    // 0 while the server did not say.
    pub total: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| (self.done.load(Ordering::Relaxed) as f32 / total as f32).clamp(0.0, 1.0))
    }
}

#[derive(Debug)]
pub enum FetchError {
    Cancelled,
    NotAllowed(String),
    Status(u32),
    TooLarge,
    Network(String),
    Io(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Cancelled => f.write_str("cancelado"),
            FetchError::NotAllowed(url) => write!(f, "dirección no permitida: {url}"),
            FetchError::Status(code) => write!(f, "el servidor respondió {code}"),
            FetchError::TooLarge => f.write_str("archivo demasiado grande"),
            FetchError::Network(e) | FetchError::Io(e) => f.write_str(e),
        }
    }
}

fn query_number(request: *mut core::ffi::c_void, what: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: `value` is a valid u32 buffer of `len` bytes.
    unsafe {
        WinHttpQueryHeaders(request, what | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some((&raw mut value).cast()), &mut len, std::ptr::null_mut())
            .ok()
            .map(|()| value)
    }
}

fn query_location(request: *mut core::ffi::c_void) -> Option<String> {
    let mut buffer = [0u16; 2048];
    let mut len = (buffer.len() * 2) as u32;
    // SAFETY: the buffer and its byte length describe the same memory.
    unsafe {
        WinHttpQueryHeaders(request, WINHTTP_QUERY_LOCATION, PCWSTR::null(), Some(buffer.as_mut_ptr().cast()), &mut len, std::ptr::null_mut()).ok()?;
    }
    Some(String::from_utf16_lossy(&buffer[..(len / 2) as usize]))
}

enum Hop {
    Redirect(String),
    Done,
}

// One GET without automatic redirects; a 2xx body goes to `sink`.
fn get_once(url: &Url, limit: u64, progress: &Progress, sink: &mut dyn Write) -> Result<Hop, FetchError> {
    let net = |what: &str, e: windows::core::Error| FetchError::Network(format!("{what}: {e}"));
    let agent = HSTRING::from(format!("RustPlay/{}", env!("CARGO_PKG_VERSION")));
    // The loopback test server must not go through a proxy.
    let access = if url.host == "127.0.0.1" { WINHTTP_ACCESS_TYPE_NO_PROXY } else { WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY };
    // SAFETY: every handle is wrapped in `Handle` and closed on drop; buffers outlive the calls.
    unsafe {
        let session = crate::update::open(WinHttpOpen(&agent, access, PCWSTR::null(), PCWSTR::null(), 0), "WinHttpOpen").map_err(FetchError::Network)?;
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000);
        let connect = crate::update::open(WinHttpConnect(session.0, &HSTRING::from(url.host.as_str()), url.port, 0), "WinHttpConnect").map_err(FetchError::Network)?;
        let flags = if url.secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let request = crate::update::open(
            WinHttpOpenRequest(connect.0, windows::core::w!("GET"), &HSTRING::from(url.path.as_str()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags),
            "WinHttpOpenRequest",
        )
        .map_err(FetchError::Network)?;
        let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
        WinHttpSetOption(Some(request.0.cast_const()), WINHTTP_OPTION_REDIRECT_POLICY, Some(&never)).map_err(|e| net("WinHttpSetOption", e))?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(|e| net("WinHttpSendRequest", e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| net("WinHttpReceiveResponse", e))?;
        let status = query_number(request.0, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            return query_location(request.0).map(Hop::Redirect).ok_or(FetchError::Status(status));
        }
        if !(200..300).contains(&status) {
            return Err(FetchError::Status(status));
        }
        let announced = u64::from(query_number(request.0, WINHTTP_QUERY_CONTENT_LENGTH).unwrap_or(0));
        if announced > limit {
            return Err(FetchError::TooLarge);
        }
        progress.total.store(announced, Ordering::Relaxed);
        progress.done.store(0, Ordering::Relaxed);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            if progress.cancel.load(Ordering::Relaxed) {
                return Err(FetchError::Cancelled);
            }
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read).map_err(|e| net("WinHttpReadData", e))?;
            if read == 0 {
                break;
            }
            total += u64::from(read);
            if total > limit {
                return Err(FetchError::TooLarge);
            }
            sink.write_all(&chunk[..read as usize]).map_err(|e| FetchError::Io(e.to_string()))?;
            progress.done.store(total, Ordering::Relaxed);
        }
        if announced > 0 && total != announced {
            return Err(FetchError::Network(String::from("descarga incompleta")));
        }
        Ok(Hop::Done)
    }
}

// GET with every URL (the first and each redirect target) checked by `allow`.
pub fn fetch(start: &str, allow: &dyn Fn(&Url) -> bool, limit: u64, progress: &Progress, sink: &mut dyn Write) -> Result<(), FetchError> {
    let mut url = parse_url(start).filter(|u| allow(u)).ok_or_else(|| FetchError::NotAllowed(start.to_owned()))?;
    for _ in 0..=MAX_REDIRECTS {
        match get_once(&url, limit, progress, sink)? {
            Hop::Done => return Ok(()),
            Hop::Redirect(location) => {
                url = resolve_location(&url, &location).filter(|u| allow(u)).ok_or(FetchError::NotAllowed(location))?;
            }
        }
    }
    Err(FetchError::Network(String::from("demasiadas redirecciones")))
}

struct HashingFile {
    file: std::fs::File,
    hasher: Sha256,
}

impl Write for HashingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write_all(buf)?;
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[derive(Debug)]
pub enum Outcome {
    // Downloaded and verified.
    Ready(PathBuf),
    NoChecksum,
    Mismatch,
    Cancelled,
    Failed(String),
}

// Downloads `installer_url` into `dir` after reading its expected digest from `checksum_url`.
// The file only gets its final name once the digest matches.
pub fn download_verified(
    installer_name: &str,
    installer_url: &str,
    checksum: Option<(&str, bool)>,
    dir: &Path,
    allow: &dyn Fn(&Url) -> bool,
    progress: &Progress,
) -> Outcome {
    let Some(name) = safe_file_name(installer_name) else { return Outcome::Failed(String::from("nombre de archivo no válido")) };
    let Some((checksum_url, dedicated)) = checksum else { return Outcome::NoChecksum };
    let mut sums = Vec::new();
    if let Err(e) = fetch(checksum_url, allow, MAX_CHECKSUMS, progress, &mut sums) {
        return match e {
            FetchError::Cancelled => Outcome::Cancelled,
            e => Outcome::Failed(e.to_string()),
        };
    }
    let Some(expected) = expected_digest(&String::from_utf8_lossy(&sums), &name, dedicated) else { return Outcome::NoChecksum };
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Outcome::Failed(e.to_string());
    }
    let part = dir.join(format!("{name}.part"));
    let target = dir.join(&name);
    let _ = std::fs::remove_file(&target);
    let file = match std::fs::File::create(&part) {
        Ok(file) => file,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let mut sink = HashingFile { file, hasher: Sha256::new() };
    let result = fetch(installer_url, allow, MAX_INSTALLER, progress, &mut sink);
    let digest: [u8; 32] = sink.hasher.finalize_reset().into();
    drop(sink);
    let outcome = match result {
        Err(FetchError::Cancelled) => Outcome::Cancelled,
        Err(e) => Outcome::Failed(e.to_string()),
        Ok(()) if digest != expected => Outcome::Mismatch,
        Ok(()) => match std::fs::rename(&part, &target) {
            Ok(()) => return Outcome::Ready(target),
            Err(e) => Outcome::Failed(e.to_string()),
        },
    };
    let _ = std::fs::remove_file(&part);
    outcome
}

pub struct Download {
    pub progress: Arc<Progress>,
    pub rx: mpsc::Receiver<Outcome>,
}

fn temp_dir(version: &str) -> PathBuf {
    std::env::temp_dir().join(format!("RustPlay-update-{}", safe_file_name(version).unwrap_or_else(|| String::from("new"))))
}

// Starts the download on its own thread; `notify` (WM_BACKEND) is posted to `hwnd` when it ends.
pub fn spawn(assets: &[Asset], version: &str, hwnd: HWND, notify: u32) -> Option<(Download, InstallerKind)> {
    let (installer, kind) = pick_installer(assets)?;
    let checksum = pick_checksum_asset(assets, &installer.name).map(|a| (a.url.clone(), a.name.eq_ignore_ascii_case(&format!("{}.sha256", installer.name))));
    let url = installer.url.clone();
    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    let shared = Arc::clone(&progress);
    let dir = temp_dir(version);
    let hwnd = hwnd.0 as isize;
    let file = installer.name.clone();
    std::thread::Builder::new()
        .name(String::from("rustplay-download"))
        .spawn(move || {
            tracing::info!("descargando la actualización {file} de {url}");
            let allow = |u: &Url| is_allowed_download_url(u);
            let outcome = download_verified(&file, &url, checksum.as_ref().map(|(u, d)| (u.as_str(), *d)), &dir, &allow, &shared);
            match &outcome {
                Outcome::Ready(path) => tracing::info!("actualización descargada y verificada: {}", path.display()),
                other => tracing::warn!("la descarga de la actualización no se completó: {other:?}"),
            }
            if tx.send(outcome).is_ok() {
                // SAFETY: posting to a window handle is allowed from any thread.
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as _)), notify, WPARAM(0), LPARAM(0));
                }
            }
        })
        .ok()?;
    Some((Download { progress, rx }, kind))
}

// Starts the installer (Windows asks for elevation itself). The caller then exits RustPlay.
pub fn launch(path: &Path, kind: InstallerKind, silent: bool) -> bool {
    let file: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    let args: Vec<u16> = install_args(kind, silent).encode_utf16().chain(std::iter::once(0)).collect();
    let parameters = if args.len() > 1 { PCWSTR(args.as_ptr()) } else { PCWSTR::null() };
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let result = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(None, windows::core::w!("open"), PCWSTR(file.as_ptr()), parameters, None, SW_SHOWNORMAL)
    };
    // ShellExecute reports success with a value above 32.
    let ok = result.0 as isize > 32;
    if !ok {
        tracing::warn!("no se pudo iniciar el instalador {}", path.display());
    }
    ok
}

// Only the classic installer can replace itself; MSIX updates through the Store and the portable
// build is replaced by hand.
pub fn can_self_update() -> bool {
    !crate::autostart::is_packaged() && !crate::autostart::is_portable()
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::{Ipv4Addr, TcpListener};

    use super::*;

    fn asset(name: &str) -> Asset {
        Asset { name: name.into(), url: format!("https://github.com/o/r/releases/download/v1/{name}"), size: 1 }
    }

    #[test]
    fn installer_selection() {
        let assets = [
            asset("RustPlay-1.0-portable.zip"),
            asset("SHA256SUMS"),
            asset("RustPlay-Setup-1.0.exe.sig"),
            asset("other.msi"),
            asset("RustPlay.msix"),
            asset("RustPlay-Setup-1.0.exe"),
            asset("RustPlay-1.0.msi"),
        ];
        let (picked, kind) = pick_installer(&assets).unwrap();
        assert_eq!((picked.name.as_str(), kind), ("RustPlay-Setup-1.0.exe", InstallerKind::Exe));
        let (picked, kind) = pick_installer(&assets[..5]).unwrap();
        assert_eq!((picked.name.as_str(), kind), ("RustPlay.msix", InstallerKind::Msix));
        assert!(pick_installer(&[asset("RustPlay-1.0-portable.zip"), asset("tool.exe"), asset("RustPlay-Setup-1.0.exe.sig")]).is_none());
        assert!(pick_installer(&[asset("rustplay-setup-2.exe")]).is_some());
    }

    #[test]
    fn checksum_asset_selection() {
        let assets = [asset("SHA256SUMS"), asset("RustPlay-Setup-1.0.exe.sha256"), asset("RustPlay-Setup-1.0.exe")];
        assert_eq!(pick_checksum_asset(&assets, "RustPlay-Setup-1.0.exe").unwrap().name, "RustPlay-Setup-1.0.exe.sha256");
        assert_eq!(pick_checksum_asset(&assets[..1], "RustPlay-Setup-1.0.exe").unwrap().name, "SHA256SUMS");
        assert!(pick_checksum_asset(&assets[2..], "RustPlay-Setup-1.0.exe").is_none());
    }

    #[test]
    fn url_allow_list() {
        for ok in [
            "https://github.com/o/r/releases/download/v1/a.exe",
            "https://objects.githubusercontent.com/x?y=1",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1",
            "https://GitHub.com:443/a",
        ] {
            assert!(is_allowed_download_text(ok), "{ok}");
        }
        for bad in [
            "http://github.com/a",
            "https://github.com.evil.example/a",
            "https://evil.example/github.com/a",
            "https://user@github.com/a",
            "https://github.com:8443/a",
            "https://api.github.com/a",
            "https://githubusercontent.com/a",
            "https://notgithub.com/a",
            "ftp://github.com/a",
            "https://github.com/a b",
            "github.com/a",
            "https:///a",
        ] {
            assert!(!is_allowed_download_text(bad), "{bad}");
        }
    }

    #[test]
    fn urls_parse_and_redirects_resolve() {
        let url = parse_url("http://127.0.0.1:8080/a/b?c=d#frag").unwrap();
        assert_eq!((url.secure, url.host.as_str(), url.port, url.path.as_str()), (false, "127.0.0.1", 8080, "/a/b?c=d"));
        assert_eq!(parse_url("https://github.com?x=1").unwrap().path, "/?x=1");
        let base = parse_url("https://github.com/a").unwrap();
        assert_eq!(resolve_location(&base, "/b").unwrap().path, "/b");
        assert_eq!(resolve_location(&base, "https://objects.githubusercontent.com/z").unwrap().host, "objects.githubusercontent.com");
        assert!(resolve_location(&base, "//evil.example/x").is_none());
        assert!(resolve_location(&base, "relative").is_none());
    }

    const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn checksum_parsing() {
        let digest = parse_hex_digest(HEX).unwrap();
        let sums = format!("{HEX}  RustPlay-Setup-1.0.exe\n{} *other.zip\r\n\nnot a line\n", "00".repeat(32));
        assert_eq!(expected_digest(&sums, "RustPlay-Setup-1.0.exe", false), Some(digest));
        assert_eq!(expected_digest(&sums, "rustplay-setup-1.0.EXE", false), Some(digest));
        assert_eq!(expected_digest(&sums, "other.zip", false), Some([0; 32]));
        assert_eq!(expected_digest(&sums, "missing.exe", false), None);
        // `*` binary marker and a path in front of the name.
        assert_eq!(expected_digest(&format!("{} *dist/a.exe", HEX.to_uppercase()), "a.exe", false), Some(digest));
        // A bare digest is accepted only from the installer's own checksum file.
        assert_eq!(expected_digest(&format!("{HEX}\n"), "a.exe", false), None);
        assert_eq!(expected_digest(&format!("{HEX}\n"), "a.exe", true), Some(digest));
        assert_eq!(expected_digest("zz", "a.exe", true), None);
        assert_eq!(parse_hex_digest(&HEX[..63]), None);
        assert_eq!(parse_hex_digest(&format!("{}g", &HEX[..63])), None);
    }

    #[test]
    fn file_verification() {
        let dir = std::env::temp_dir().join(format!("rustplay-updater-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert!(verify_file(&path, &parse_hex_digest(HEX).unwrap()).unwrap());
        assert!(!verify_file(&path, &[1; 32]).unwrap());
        assert!(verify_file(&dir.join("missing"), &[1; 32]).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn safe_names_and_flags() {
        assert_eq!(safe_file_name("RustPlay-Setup-1.0.exe").as_deref(), Some("RustPlay-Setup-1.0.exe"));
        for bad in ["", "../x.exe", "a b.exe", "a\\b.exe", ".hidden", "a;b"] {
            assert_eq!(safe_file_name(bad), None, "{bad}");
        }
        assert_eq!(install_args(InstallerKind::Exe, true), "/SILENT /CLOSEAPPLICATIONS /RESTARTAPPLICATIONS");
        assert_eq!(install_args(InstallerKind::Exe, false), "/CLOSEAPPLICATIONS /RESTARTAPPLICATIONS");
        assert_eq!(install_args(InstallerKind::Msi, true), "");
    }

    // Minimal HTTP server on the loopback interface; routes: /ok, /redirect (to /ok), /away
    // (to another host), /missing (404).
    fn server(body: &'static [u8]) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                std::thread::spawn(move || {
                    let mut request = [0u8; 2048];
                    let n = stream.read(&mut request).unwrap_or(0);
                    let text = String::from_utf8_lossy(&request[..n]).into_owned();
                    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let reply = match path.as_str() {
                        "/ok" => {
                            let mut head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                            head.extend_from_slice(body);
                            head
                        }
                        "/sums" => {
                            let line = format!("{HEX}  a.exe\n");
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{line}", line.len()).into_bytes()
                        }
                        "/redirect" => b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/away" => b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&reply);
                });
            }
        });
        port
    }

    fn loopback(u: &Url) -> bool {
        u.host == "127.0.0.1"
    }

    #[test]
    fn fetch_follows_checked_redirects_and_reports_progress() {
        let port = server(b"abc");
        let progress = Progress::default();
        let mut out = Vec::new();
        fetch(&format!("http://127.0.0.1:{port}/redirect"), &loopback, 1024, &progress, &mut out).unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(progress.done.load(Ordering::Relaxed), 3);
        assert_eq!(progress.fraction(), Some(1.0));
        // A redirect to a host outside the allow-list is refused.
        let err = fetch(&format!("http://127.0.0.1:{port}/away"), &loopback, 1024, &progress, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FetchError::NotAllowed(_)), "{err}");
        let err = fetch(&format!("http://127.0.0.1:{port}/missing"), &loopback, 1024, &progress, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FetchError::Status(404)), "{err}");
        let err = fetch(&format!("http://127.0.0.1:{port}/ok"), &loopback, 2, &progress, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FetchError::TooLarge), "{err}");
        // The production allow-list rejects a loopback URL.
        let err = fetch(&format!("http://127.0.0.1:{port}/ok"), &is_allowed_download_url, 1024, &progress, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FetchError::NotAllowed(_)));
        progress.cancel.store(true, Ordering::Relaxed);
        let err = fetch(&format!("http://127.0.0.1:{port}/ok"), &loopback, 1024, &progress, &mut Vec::new()).unwrap_err();
        assert!(matches!(err, FetchError::Cancelled));
    }

    #[test]
    fn verified_download_end_to_end() {
        let port = server(b"abc");
        let base = format!("http://127.0.0.1:{port}");
        let dir = std::env::temp_dir().join(format!("rustplay-dl-{port}"));
        let progress = Progress::default();
        // The digest in /sums is the one of "abc".
        let ready = download_verified("a.exe", &format!("{base}/ok"), Some((&format!("{base}/sums"), false)), &dir, &loopback, &progress);
        let Outcome::Ready(path) = ready else { panic!("{ready:?}") };
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(!dir.join("a.exe.part").exists());
        std::fs::remove_dir_all(&dir).ok();

        // No checksum asset: refused before anything is downloaded.
        let outcome = download_verified("a.exe", &format!("{base}/ok"), None, &dir, &loopback, &progress);
        assert!(matches!(outcome, Outcome::NoChecksum));
        assert!(!dir.join("a.exe").exists());

        // A different body does not match the digest: nothing is left behind.
        let other = server(b"tampered");
        let outcome = download_verified(
            "a.exe",
            &format!("http://127.0.0.1:{other}/ok"),
            Some((&format!("http://127.0.0.1:{other}/sums"), false)),
            &dir,
            &loopback,
            &progress,
        );
        assert!(matches!(outcome, Outcome::Mismatch), "{outcome:?}");
        assert!(!dir.join("a.exe").exists() && !dir.join("a.exe.part").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
