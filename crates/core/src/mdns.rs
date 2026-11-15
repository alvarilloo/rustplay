use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::NetworkManagement::Dns::{
    DNS_QUERY_REQUEST_VERSION1, DNS_RECORDW, DNS_SERVICE_BROWSE_REQUEST, DNS_SERVICE_CANCEL,
    DNS_SERVICE_INSTANCE, DNS_SERVICE_REGISTER_REQUEST, DNS_SERVICE_RESOLVE_REQUEST, DNS_TYPE_PTR,
    DnsFree, DnsFreeRecordList, DnsServiceBrowse, DnsServiceBrowseCancel,
    DnsServiceConstructInstance, DnsServiceDeRegister, DnsServiceFreeInstance, DnsServiceRegister,
    DnsServiceResolve,
};
use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows::core::{HRESULT, PCWSTR, PWSTR};

use crate::{Error, Result};

const DNS_REQUEST_PENDING: u32 = 9506;
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const BROWSE_QUEUE: usize = 64;

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub instance: String,
    pub service_type: &'static str,
    pub port: u16,
    pub txt: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub instance: String,
    pub service_type: String,
    pub host: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub txt: HashMap<String, String>,
}

impl DiscoveredDevice {
    pub fn display_name(&self) -> &str {
        self.instance
            .split_once('@')
            .map_or(&self.instance, |(_, name)| name)
    }

    pub fn device_id(&self) -> Option<&str> {
        self.txt.get("deviceid").map(String::as_str)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn win_error(status: u32) -> Error {
    Error::Windows(windows::core::Error::from_hresult(HRESULT::from_win32(
        status,
    )))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn pwstr_to_string(p: PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the pointer is non-null and the API guarantees a NUL-terminated wide string.
    unsafe { p.to_string().ok() }
}

fn local_host_name() -> Result<String> {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: buf is valid for `len` u16 elements and len is a valid in/out pointer.
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )?
    };
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}

// One-shot signal with a timed wait, used to learn that a cancel/deregister completed.
#[derive(Default)]
struct Signal {
    done: Mutex<bool>,
    cv: Condvar,
}

impl Signal {
    fn set(&self) {
        *lock(&self.done) = true;
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.done);
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

pub fn parse_txt_entry(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (entry.to_string(), String::new()),
    }
}

// "AA@Name._raop._tcp.local" -> "AA@Name"
pub fn split_instance(full: &str, service_type: &str) -> Option<String> {
    let trimmed = full.strip_suffix('.').unwrap_or(full);
    let suffix = format!(".{service_type}.local");
    let cut = trimmed.len().checked_sub(suffix.len())?;
    if !trimmed.is_char_boundary(cut) || !trimmed[cut..].eq_ignore_ascii_case(&suffix) {
        return None;
    }
    let instance = &trimmed[..cut];
    (!instance.is_empty()).then(|| instance.to_string())
}

// Returns true when the device is new or differs from the last emitted one.
fn should_emit(seen: &mut HashMap<String, DiscoveredDevice>, mut dev: DiscoveredDevice) -> bool {
    dev.addrs.sort();
    dev.addrs.dedup();
    match seen.get(&dev.instance) {
        Some(prev) if *prev == dev => false,
        _ => {
            seen.insert(dev.instance.clone(), dev);
            true
        }
    }
}

struct RegisterCtx {
    tx: Mutex<Option<oneshot::Sender<u32>>>,
}

pub struct Registration {
    instance: *mut DNS_SERVICE_INSTANCE,
    ctx: *const RegisterCtx,
}

// SAFETY: the raw pointers are only used for the DNS-SD calls (thread-agnostic) and in Drop.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

unsafe extern "system" fn register_callback(
    status: u32,
    ctx: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    // SAFETY: ctx comes from Arc::into_raw and stays alive until deregistration completes.
    let ctx = unsafe { &*(ctx as *const RegisterCtx) };
    if let Some(tx) = lock(&ctx.tx).take() {
        let _ = tx.send(status);
    }
    if !instance.is_null() {
        // SAFETY: the API hands over ownership of the returned instance copy.
        unsafe { DnsServiceFreeInstance(instance) };
    }
}

unsafe extern "system" fn deregister_callback(
    _status: u32,
    ctx: *const c_void,
    _instance: *const DNS_SERVICE_INSTANCE,
) {
    // SAFETY: ctx comes from Arc::into_raw and is released by the waiting Drop (or leaked).
    let signal = unsafe { &*(ctx as *const Signal) };
    signal.set();
}

fn register_request(
    instance: *mut DNS_SERVICE_INSTANCE,
    callback: windows::Win32::NetworkManagement::Dns::PDNS_SERVICE_REGISTER_COMPLETE,
    ctx: *mut c_void,
) -> DNS_SERVICE_REGISTER_REQUEST {
    DNS_SERVICE_REGISTER_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        pServiceInstance: instance,
        pRegisterCompletionCallback: callback,
        pQueryContext: ctx,
        ..Default::default()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let signal = Arc::new(Signal::default());
        let signal_raw = Arc::into_raw(Arc::clone(&signal));
        let request = register_request(
            self.instance,
            Some(deregister_callback),
            signal_raw as *mut c_void,
        );
        // SAFETY: the request points to a live instance; the context outlives the call.
        let status = unsafe { DnsServiceDeRegister(&request, None) };
        let completed = if status == DNS_REQUEST_PENDING {
            signal.wait(TEARDOWN_TIMEOUT)
        } else {
            // Not pending: the callback will never fire.
            true
        };
        if status != DNS_REQUEST_PENDING {
            // SAFETY: balances the into_raw above; no callback can reference it.
            drop(unsafe { Arc::from_raw(signal_raw) });
        } else if completed {
            // SAFETY: the callback has already run and will not touch the context again.
            drop(unsafe { Arc::from_raw(signal_raw) });
        }
        if completed {
            // SAFETY: deregistration finished, so Windows no longer uses the instance or the
            // registration context; both were created by this module.
            unsafe {
                DnsServiceFreeInstance(self.instance);
                drop(Arc::from_raw(self.ctx));
            }
        } else {
            tracing::warn!(
                "mDNS: la baja del servicio no terminó a tiempo; se conservan los recursos"
            );
        }
    }
}

pub async fn register(spec: ServiceSpec) -> Result<Registration> {
    let host = format!("{}.local", local_host_name()?);
    let full_name = format!("{}.{}.local", spec.instance, spec.service_type);
    let name_w = wide(&full_name);
    let host_w = wide(&host);
    let keys_w: Vec<Vec<u16>> = spec.txt.iter().map(|(k, _)| wide(k)).collect();
    let values_w: Vec<Vec<u16>> = spec.txt.iter().map(|(_, v)| wide(v)).collect();
    let keys: Vec<PCWSTR> = keys_w.iter().map(|k| PCWSTR(k.as_ptr())).collect();
    let values: Vec<PCWSTR> = values_w.iter().map(|v| PCWSTR(v.as_ptr())).collect();

    // SAFETY: all pointers reference live NUL-terminated buffers; the API copies the data.
    let instance = unsafe {
        DnsServiceConstructInstance(
            PCWSTR(name_w.as_ptr()),
            PCWSTR(host_w.as_ptr()),
            None,
            None,
            spec.port,
            0,
            0,
            keys.len() as u32,
            keys.as_ptr(),
            values.as_ptr(),
        )
    };
    if instance.is_null() {
        return Err(Error::protocol(
            "no se pudo construir la instancia del servicio mDNS",
        ));
    }

    let (tx, rx) = oneshot::channel();
    let ctx = Arc::into_raw(Arc::new(RegisterCtx {
        tx: Mutex::new(Some(tx)),
    }));
    // From here on, Drop of Registration owns the cleanup of both resources.
    let registration = Registration { instance, ctx };

    let request = register_request(instance, Some(register_callback), ctx as *mut c_void);
    // SAFETY: the request and the instance are valid; the context lives in `registration`.
    let status = unsafe { DnsServiceRegister(&request, None) };
    if status != DNS_REQUEST_PENDING && status != 0 {
        // The callback will not fire; release directly instead of waiting for a deregister.
        // SAFETY: nothing else references these resources after a failed submission.
        unsafe {
            DnsServiceFreeInstance(registration.instance);
            drop(Arc::from_raw(registration.ctx));
        }
        std::mem::forget(registration);
        return Err(win_error(status));
    }

    match tokio::time::timeout(REGISTER_TIMEOUT, rx).await {
        Ok(Ok(0)) => Ok(registration),
        Ok(Ok(code)) => Err(win_error(code)),
        Ok(Err(_)) => Err(Error::protocol("registro mDNS cancelado")),
        Err(_) => Err(Error::Timeout),
    }
}

struct BrowseShared {
    tx: mpsc::Sender<DiscoveredDevice>,
    service_type: &'static str,
    seen: Mutex<HashMap<String, DiscoveredDevice>>,
    resolving: Mutex<HashSet<String>>,
    cancelled: Signal,
    closed: AtomicBool,
}

struct ResolveCtx {
    shared: Arc<BrowseShared>,
    full_name: String,
}

pub struct Browser {
    events: mpsc::Receiver<DiscoveredDevice>,
    shared: *const BrowseShared,
    cancel: Box<DNS_SERVICE_CANCEL>,
    _query: Vec<u16>,
}

// SAFETY: the raw pointers are only used for thread-safe DNS-SD calls and in Drop.
unsafe impl Send for Browser {}
unsafe impl Sync for Browser {}

impl Browser {
    pub async fn next(&mut self) -> Option<DiscoveredDevice> {
        self.events.recv().await
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // SAFETY: shared is alive (we still own its raw strong count).
        let shared = unsafe { &*self.shared };
        shared.closed.store(true, Ordering::Release);
        // SAFETY: the cancel handle was filled by DnsServiceBrowse and is still alive.
        let status = unsafe { DnsServiceBrowseCancel(&*self.cancel) };
        let completed = status != 0 || shared.cancelled.wait(TEARDOWN_TIMEOUT);
        if completed {
            // SAFETY: the cancellation callback ran (or the cancel failed, so no more callbacks
            // are expected); this releases the strong count given to the API.
            drop(unsafe { Arc::from_raw(self.shared) });
        } else {
            tracing::warn!(
                "mDNS: la cancelación de la búsqueda no terminó a tiempo; se conserva el contexto"
            );
        }
    }
}

fn device_from_instance(
    shared: &BrowseShared,
    full_name: &str,
    inst: &DNS_SERVICE_INSTANCE,
) -> Option<DiscoveredDevice> {
    let name = pwstr_to_string(inst.pszInstanceName).unwrap_or_else(|| full_name.to_string());
    let instance = split_instance(&name, shared.service_type)?;
    let host = pwstr_to_string(inst.pszHostName)?;
    let host = host.strip_suffix('.').unwrap_or(&host).to_string();

    let mut addrs = Vec::new();
    if !inst.ip4Address.is_null() {
        // SAFETY: non-null pointer to a u32 owned by the instance; the value is in network order.
        let raw = unsafe { *inst.ip4Address };
        addrs.push(IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes())));
    }
    if !inst.ip6Address.is_null() {
        // SAFETY: non-null pointer to an IP6_ADDRESS owned by the instance; any byte pattern is valid.
        let bytes = unsafe { (*inst.ip6Address).IP6Byte };
        addrs.push(IpAddr::V6(Ipv6Addr::from(bytes)));
    }

    let mut txt = HashMap::new();
    for i in 0..inst.dwPropertyCount as usize {
        if inst.keys.is_null() {
            break;
        }
        // SAFETY: keys/values have dwPropertyCount entries per the API contract.
        let key = pwstr_to_string(unsafe { *inst.keys.add(i) });
        let value = if inst.values.is_null() {
            None
        } else {
            pwstr_to_string(unsafe { *inst.values.add(i) })
        };
        let Some(key) = key else { continue };
        match value {
            Some(v) => {
                txt.insert(key, v);
            }
            None => {
                let (k, v) = parse_txt_entry(&key);
                txt.insert(k, v);
            }
        }
    }

    if addrs.is_empty() || inst.wPort == 0 {
        return None;
    }
    Some(DiscoveredDevice {
        instance,
        service_type: shared.service_type.to_string(),
        host,
        addrs,
        port: inst.wPort,
        txt,
    })
}

unsafe extern "system" fn resolve_callback(
    status: u32,
    ctx: *const c_void,
    instance: *const DNS_SERVICE_INSTANCE,
) {
    // SAFETY: ctx comes from Box::into_raw in start_resolve and the API calls this exactly once.
    let ctx = unsafe { Box::from_raw(ctx as *mut ResolveCtx) };
    let shared = &ctx.shared;
    lock(&shared.resolving).remove(&ctx.full_name);
    if instance.is_null() {
        return;
    }
    if status == 0 && !shared.closed.load(Ordering::Acquire) {
        // SAFETY: the instance is valid until freed below.
        let dev = device_from_instance(shared, &ctx.full_name, unsafe { &*instance });
        if let Some(dev) = dev {
            let mut seen = lock(&shared.seen);
            let key = dev.instance.clone();
            let emit = dev.clone();
            if should_emit(&mut seen, dev) && shared.tx.try_send(emit).is_err() {
                seen.remove(&key);
            }
        }
    }
    // SAFETY: we own the returned instance.
    unsafe { DnsServiceFreeInstance(instance) };
}

fn start_resolve(shared: &Arc<BrowseShared>, full_name: String) {
    if !lock(&shared.resolving).insert(full_name.clone()) {
        return;
    }
    let mut name_w = wide(&full_name);
    let ctx = Box::into_raw(Box::new(ResolveCtx {
        shared: Arc::clone(shared),
        full_name,
    }));
    let request = DNS_SERVICE_RESOLVE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PWSTR(name_w.as_mut_ptr()),
        pResolveCompletionCallback: Some(resolve_callback),
        pQueryContext: ctx as *mut c_void,
    };
    let mut cancel = DNS_SERVICE_CANCEL::default();
    // SAFETY: request and name buffer are valid for the call; the API copies the name.
    let status = unsafe { DnsServiceResolve(&request, &mut cancel) };
    if status != DNS_REQUEST_PENDING as i32 {
        // SAFETY: the callback will not fire, so the context is still exclusively ours.
        let ctx = unsafe { Box::from_raw(ctx) };
        lock(&shared.resolving).remove(&ctx.full_name);
    }
}

unsafe extern "system" fn browse_callback(
    status: u32,
    ctx: *const c_void,
    records: *const DNS_RECORDW,
) {
    // SAFETY: ctx comes from Arc::into_raw and is kept alive until the cancel completes.
    let shared_ptr = ctx as *const BrowseShared;
    if status == ERROR_CANCELLED.0 {
        // SAFETY: as above.
        unsafe { (*shared_ptr).cancelled.set() };
    } else if status == 0 && !unsafe { &*shared_ptr }.closed.load(Ordering::Acquire) {
        // SAFETY: the strong count is held by the Browser; take an extra one for the helper.
        let shared = unsafe {
            Arc::increment_strong_count(shared_ptr);
            Arc::from_raw(shared_ptr)
        };
        let mut rec = records;
        while !rec.is_null() {
            // SAFETY: the list is a valid DNS_RECORD chain until freed below.
            let r = unsafe { &*rec };
            // TTL 0 marks a goodbye record; ignore it.
            if r.wType == DNS_TYPE_PTR.0 && r.dwTtl != 0 {
                // SAFETY: wType is PTR, so the PTR variant of the union is active.
                if let Some(target) = pwstr_to_string(unsafe { r.Data.PTR.pNameHost }) {
                    start_resolve(&shared, target);
                }
            }
            rec = r.pNext;
        }
    }
    if !records.is_null() {
        // SAFETY: the callback owns the record list and must free it.
        unsafe { DnsFree(Some(records as *const c_void), DnsFreeRecordList) };
    }
}

pub fn browse(service_type: &'static str) -> Result<Browser> {
    let (tx, rx) = mpsc::channel(BROWSE_QUEUE);
    let shared = Arc::new(BrowseShared {
        tx,
        service_type,
        seen: Mutex::new(HashMap::new()),
        resolving: Mutex::new(HashSet::new()),
        cancelled: Signal::default(),
        closed: AtomicBool::new(false),
    });
    let shared_raw = Arc::into_raw(shared);
    let query = wide(&format!("{service_type}.local"));
    let mut cancel = Box::new(DNS_SERVICE_CANCEL::default());
    let mut request = DNS_SERVICE_BROWSE_REQUEST {
        Version: DNS_QUERY_REQUEST_VERSION1.0,
        InterfaceIndex: 0,
        QueryName: PCWSTR(query.as_ptr()),
        pQueryContext: shared_raw as *mut c_void,
        ..Default::default()
    };
    request.Anonymous.pBrowseCallback = Some(browse_callback);
    // SAFETY: request and query are valid; the cancel handle is boxed and outlives the browse.
    let status = unsafe { DnsServiceBrowse(&request, &mut *cancel) };
    if status != DNS_REQUEST_PENDING as i32 && status != 0 {
        // SAFETY: the browse never started, so we still own the only strong count.
        drop(unsafe { Arc::from_raw(shared_raw) });
        return Err(win_error(status as u32));
    }
    Ok(Browser {
        events: rx,
        shared: shared_raw,
        cancel,
        _query: query,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(instance: &str, port: u16, addr: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            instance: instance.into(),
            service_type: "_airplay._tcp".into(),
            host: "h.local".into(),
            addrs: vec![addr.parse().unwrap()],
            port,
            txt: HashMap::new(),
        }
    }

    #[test]
    fn txt_entries() {
        assert_eq!(parse_txt_entry("a=b"), ("a".into(), "b".into()));
        assert_eq!(parse_txt_entry("a="), ("a".into(), String::new()));
        assert_eq!(parse_txt_entry("flag"), ("flag".into(), String::new()));
        assert_eq!(parse_txt_entry("k=a=b"), ("k".into(), "a=b".into()));
    }

    #[test]
    fn instance_split() {
        assert_eq!(
            split_instance("AABBCCDDEEFF@Sala._raop._tcp.local", "_raop._tcp").as_deref(),
            Some("AABBCCDDEEFF@Sala")
        );
        assert_eq!(
            split_instance("Mi TV._airplay._tcp.LOCAL.", "_airplay._tcp").as_deref(),
            Some("Mi TV")
        );
        assert_eq!(split_instance("x._raop._tcp.local", "_airplay._tcp"), None);
        assert_eq!(
            split_instance("._airplay._tcp.local", "_airplay._tcp"),
            None
        );
    }

    #[test]
    fn dedupe() {
        let mut seen = HashMap::new();
        assert!(should_emit(&mut seen, dev("a", 7000, "10.0.0.2")));
        assert!(!should_emit(&mut seen, dev("a", 7000, "10.0.0.2")));
        assert!(should_emit(&mut seen, dev("a", 7001, "10.0.0.2")));
        assert!(should_emit(&mut seen, dev("a", 7001, "10.0.0.3")));
        assert!(should_emit(&mut seen, dev("b", 7001, "10.0.0.3")));
        let mut changed = dev("b", 7001, "10.0.0.3");
        changed.txt.insert("k".into(), "v".into());
        assert!(should_emit(&mut seen, changed));
    }

    #[test]
    fn device_names() {
        let d = dev("AABB@Sala", 1, "10.0.0.2");
        assert_eq!(d.display_name(), "Sala");
        assert_eq!(d.device_id(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn mdns_register_and_browse_roundtrip() {
        const TYPE: &str = "_aircoretest._tcp";
        let mut browser = browse(TYPE).expect("browse");
        let reg = register(ServiceSpec {
            instance: "AABBCCDDEEFF@Prueba Core".into(),
            service_type: TYPE,
            port: 47_999,
            txt: vec![
                ("deviceid".into(), "AA:BB:CC:DD:EE:FF".into()),
                ("empty".into(), String::new()),
            ],
        })
        .await
        .expect("register");

        let found = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(d) = browser.next().await {
                if d.instance == "AABBCCDDEEFF@Prueba Core" {
                    return Some(d);
                }
            }
            None
        })
        .await
        .expect("timeout")
        .expect("closed");
        println!("encontrado: {found:?}");
        assert_eq!(found.port, 47_999);
        assert_eq!(found.device_id(), Some("AA:BB:CC:DD:EE:FF"));
        assert!(!found.addrs.is_empty());
        let start = std::time::Instant::now();
        drop(reg);
        drop(browser);
        println!("teardown: {:?}", start.elapsed());
    }
}
