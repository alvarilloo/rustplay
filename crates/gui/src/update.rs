// Looks for a newer RustPlay on the project's GitHub releases. It only reads the public
// release metadata; downloading and installing is up to updater.rs, on the user's click.

use std::cmp::Ordering;
use std::sync::mpsc;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::{HSTRING, PCWSTR, w};

const API_HOST: &str = "api.github.com";
const MAX_BODY: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    // The newest published release, or None when the repository has none yet.
    Latest(Option<Release>),
    Failed(String),
}

// Owner and repository of a "https://github.com/<owner>/<repo>" link; None for the placeholder.
pub fn github_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.strip_prefix("github.com/")?;
    let mut parts = rest.split(['/', '?', '#']).filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (valid(owner) && valid(repo)).then(|| (owner.to_owned(), repo.to_owned()))
}

pub fn configured_repo() -> Option<(String, String)> {
    github_repo(crate::links::GITHUB)
}

#[derive(Debug, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

fn parse_version(text: &str) -> Option<Version> {
    let text = text.trim();
    let text = text.strip_prefix(['v', 'V']).unwrap_or(text);
    let text = text.split('+').next().unwrap_or(text);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let mut numbers = [0u64; 3];
    let mut count = 0;
    for part in core.split('.') {
        if count == 3 || part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        numbers[count] = part.parse().ok()?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let pre = match pre {
        Some("") => return None,
        Some(pre) => pre.split('.').map(str::to_owned).collect(),
        None => Vec::new(),
    };
    Some(Version { core: numbers, pre })
}

fn compare_pre(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    for (x, y) in a.iter().zip(b) {
        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

// True when `candidate` (a release tag such as "v1.2.0") is a later semantic version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(a), Some(b)) = (parse_version(candidate), parse_version(current)) else { return false };
    a.core.cmp(&b.core).then_with(|| compare_pre(&a.pre, &b.pre)) == Ordering::Greater
}

// Reads the body of GET /repos/{owner}/{repo}/releases/latest. The page link must belong to the
// repository; anything else is replaced by its releases page.
pub fn parse_release(json: &str, owner: &str, repo: &str) -> Option<Release> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("draft").and_then(serde_json::Value::as_bool) == Some(true)
        || value.get("prerelease").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return None;
    }
    let tag = value.get("tag_name")?.as_str()?.trim();
    parse_version(tag)?;
    let version = tag.strip_prefix(['v', 'V']).unwrap_or(tag).to_owned();
    let base = format!("https://github.com/{owner}/{repo}/");
    let url = value
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .filter(|u| u.to_ascii_lowercase().starts_with(&base.to_ascii_lowercase()) && !u.contains(char::is_whitespace))
        .map_or_else(|| format!("{base}releases/latest"), str::to_owned);
    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_owned(),
                        url: a.get("browser_download_url")?.as_str()?.to_owned(),
                        size: a.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Release { version, url, assets })
}

pub(crate) struct Handle(pub(crate) *mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

pub(crate) fn open(handle: *mut core::ffi::c_void, what: &str) -> Result<Handle, String> {
    if handle.is_null() {
        Err(format!("{what}: {}", windows::core::Error::from_thread()))
    } else {
        Ok(Handle(handle))
    }
}

// HTTPS GET with WinHTTP; returns the status code and the body.
fn get(host: &str, path: &str) -> Result<(u32, Vec<u8>), String> {
    let agent = HSTRING::from(format!("RustPlay/{}", env!("CARGO_PKG_VERSION")));
    unsafe {
        let session = open(
            WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0),
            "WinHttpOpen",
        )?;
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 15_000);
        let connect = open(WinHttpConnect(session.0, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0), "WinHttpConnect")?;
        let request = open(
            WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "WinHttpOpenRequest",
        )?;
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n".encode_utf16().collect();
        let _ = WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD);
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
        let mut body = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read)
                .map_err(|e| format!("WinHttpReadData: {e}"))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_BODY {
                return Err(String::from("respuesta demasiado grande"));
            }
        }
        Ok((status, body))
    }
}

fn check(owner: &str, repo: &str) -> Outcome {
    let path = format!("/repos/{owner}/{repo}/releases/latest");
    match get(API_HOST, &path) {
        Ok((200, body)) => match parse_release(&String::from_utf8_lossy(&body), owner, repo) {
            Some(release) => Outcome::Latest(Some(release)),
            None => Outcome::Failed(String::from("respuesta de GitHub no válida")),
        },
        Ok((404, _)) => Outcome::Latest(None),
        Ok((status, _)) => Outcome::Failed(format!("GitHub respondió {status}")),
        Err(e) => Outcome::Failed(e),
    }
}

// Runs one check on its own thread; the result arrives through the returned channel and
// `notify` (WM_BACKEND) is posted to `hwnd` when it is ready.
pub fn spawn(hwnd: HWND, notify: u32) -> Option<mpsc::Receiver<Outcome>> {
    let (owner, repo) = configured_repo()?;
    let (tx, rx) = mpsc::channel();
    let hwnd = hwnd.0 as isize;
    let spawned = std::thread::Builder::new().name(String::from("rustplay-update")).spawn(move || {
        tracing::info!("buscando actualizaciones en {API_HOST} ({owner}/{repo})");
        let outcome = check(&owner, &repo);
        match &outcome {
            Outcome::Latest(Some(r)) => tracing::info!("última versión publicada: {} ({})", r.version, r.url),
            Outcome::Latest(None) => tracing::info!("el repositorio aún no tiene versiones publicadas"),
            Outcome::Failed(e) => tracing::warn!("no se pudo buscar actualizaciones en {API_HOST}: {e}"),
        }
        if tx.send(outcome).is_ok() {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), notify, WPARAM(0), LPARAM(0));
            }
        }
    });
    spawned.ok().map(|_| rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_links() {
        assert_eq!(github_repo("https://github.com/"), None);
        assert_eq!(github_repo("https://github.com/someone"), None);
        assert_eq!(github_repo("https://github.com/someone/rustplay"), Some(("someone".into(), "rustplay".into())));
        assert_eq!(github_repo("https://github.com/someone/rustplay.git/"), Some(("someone".into(), "rustplay".into())));
        assert_eq!(github_repo("https://www.github.com/a-b/c_d.e/tree/main"), Some(("a-b".into(), "c_d.e".into())));
        assert_eq!(github_repo("https://gitlab.com/a/b"), None);
        assert_eq!(github_repo("https://github.com/a b/c"), None);
    }

    #[test]
    fn version_order() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(is_newer("1.0.0", "1.0.0-beta.2"));
        assert!(!is_newer("1.0.0-beta.2", "1.0.0"));
        assert!(is_newer("1.0.0-beta.11", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-rc.1", "1.0.0-beta"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(!is_newer("1.0.0+build.5", "1.0.0"));
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("1.x.0", "0.1.0"));
        assert!(!is_newer("1.2.3.4", "0.1.0"));
    }

    #[test]
    fn release_json() {
        let json = r#"{"url":"x","html_url":"https://github.com/me/rustplay/releases/tag/v0.3.0",
            "tag_name":"v0.3.0","name":"RustPlay 0.3","draft":false,"prerelease":false,"assets":[]}"#;
        assert_eq!(
            parse_release(json, "me", "rustplay"),
            Some(Release { version: "0.3.0".into(), url: "https://github.com/me/rustplay/releases/tag/v0.3.0".into(), assets: vec![] })
        );
        let with_assets = r#"{"tag_name":"v1.0.0","assets":[
            {"name":"RustPlay-Setup-1.0.0.exe","browser_download_url":"https://github.com/me/rustplay/releases/download/v1.0.0/RustPlay-Setup-1.0.0.exe","size":1234},
            {"name":"broken"}]}"#;
        let release = parse_release(with_assets, "me", "rustplay").unwrap();
        assert_eq!(release.assets.len(), 1);
        assert_eq!((release.assets[0].name.as_str(), release.assets[0].size), ("RustPlay-Setup-1.0.0.exe", 1234));
        let foreign = r#"{"html_url":"https://evil.example/x","tag_name":"1.0.0"}"#;
        assert_eq!(
            parse_release(foreign, "me", "rustplay").map(|r| r.url),
            Some("https://github.com/me/rustplay/releases/latest".into())
        );
        assert_eq!(parse_release(r#"{"tag_name":"v2.0.0","prerelease":true}"#, "me", "rustplay"), None);
        assert_eq!(parse_release(r#"{"tag_name":"nightly"}"#, "me", "rustplay"), None);
        assert_eq!(parse_release(r#"{"message":"Not Found"}"#, "me", "rustplay"), None);
        assert_eq!(parse_release("not json", "me", "rustplay"), None);
    }
}
