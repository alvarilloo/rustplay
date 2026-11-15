// First-run setup assistant / system check. The decisions (what a finding means, the report
// text) are pure functions at the top of the file and are unit-tested; the Windows calls that
// gather the facts sit behind small functions below. Nothing here changes the system except
// `add_firewall_rule`, which only runs when the user clicks its button (UAC prompt).

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Warning,
    Problem,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CheckId {
    Network,
    Firewall,
    Mdns,
    Gpu,
    Audio,
    Port,
}

impl CheckId {
    pub const ALL: [CheckId; 6] = [CheckId::Network, CheckId::Firewall, CheckId::Mdns, CheckId::Gpu, CheckId::Audio, CheckId::Port];

    pub fn title_key(self) -> &'static str {
        match self {
            CheckId::Network => "setup.network.title",
            CheckId::Firewall => "setup.firewall.title",
            CheckId::Mdns => "setup.mdns.title",
            CheckId::Gpu => "setup.gpu.title",
            CheckId::Audio => "setup.audio.title",
            CheckId::Port => "setup.port.title",
        }
    }
}

// What the button of a finding does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fix {
    OpenNetworkSettings,
    AddFirewallRule,
    OpenFirewall,
}

impl Fix {
    pub fn label_key(self) -> &'static str {
        match self {
            Fix::OpenNetworkSettings => "setup.fix.network",
            Fix::AddFirewallRule => "setup.fix.firewall",
            Fix::OpenFirewall => "setup.fix.open_firewall",
        }
    }
}

// A finding: `message` is a language key whose `{detail}` placeholder receives `detail`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub id: CheckId,
    pub status: Status,
    pub message: &'static str,
    pub detail: String,
    pub fix: Option<Fix>,
}

impl CheckResult {
    fn new(id: CheckId, status: Status, message: &'static str) -> Self {
        Self { id, status, message, detail: String::new(), fix: None }
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    fn fix(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }
}

// The worst status of a list; Ok for an empty one.
pub fn overall(results: &[CheckResult]) -> Status {
    results.iter().map(|r| r.status).fold(Status::Ok, |worst, s| match (worst, s) {
        (Status::Problem, _) | (_, Status::Problem) => Status::Problem,
        (Status::Warning, _) | (_, Status::Warning) => Status::Warning,
        _ => Status::Ok,
    })
}

pub fn count(results: &[CheckResult], status: Status) -> usize {
    results.iter().filter(|r| r.status == status).count()
}

// ---------------------------------------------------------------- network profile

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetCategory {
    Public,
    Private,
    Domain,
}

pub fn classify_network(connected: &[(String, NetCategory)]) -> CheckResult {
    let id = CheckId::Network;
    if connected.is_empty() {
        return CheckResult::new(id, Status::Warning, "setup.network.none");
    }
    let public: Vec<&str> = connected.iter().filter(|(_, c)| *c == NetCategory::Public).map(|(n, _)| n.as_str()).collect();
    if public.is_empty() {
        let names: Vec<&str> = connected.iter().map(|(n, _)| n.as_str()).collect();
        return CheckResult::new(id, Status::Ok, "setup.network.ok").detail(names.join(", "));
    }
    CheckResult::new(id, Status::Warning, "setup.network.public").detail(public.join(", ")).fix(Fix::OpenNetworkSettings)
}

// ---------------------------------------------------------------- firewall

pub const PROFILE_PRIVATE: i32 = 2;
pub const PROTO_ANY: i32 = 256;
pub const PROTO_TCP: i32 = 6;
pub const PROTO_UDP: i32 = 17;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleInfo {
    pub app: String,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    pub profiles: i32,
    pub protocol: i32,
}

fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_matches('"').replace('/', "\\").to_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

pub fn classify_firewall(rules: &[RuleInfo], exe: &str, private_enabled: bool) -> CheckResult {
    let id = CheckId::Firewall;
    let mine = rules.iter().filter(|r| r.enabled && r.inbound && r.profiles & PROFILE_PRIVATE != 0 && same_path(&r.app, exe));
    let mut allowed = (false, false);
    for rule in mine {
        let (tcp, udp) = (matches!(rule.protocol, PROTO_ANY | PROTO_TCP), matches!(rule.protocol, PROTO_ANY | PROTO_UDP));
        if !rule.allow {
            if tcp || udp {
                return CheckResult::new(id, Status::Problem, "setup.firewall.blocked").fix(Fix::OpenFirewall);
            }
            continue;
        }
        allowed.0 |= tcp;
        allowed.1 |= udp;
    }
    match allowed {
        (true, true) => CheckResult::new(id, Status::Ok, "setup.firewall.ok"),
        _ if !private_enabled => CheckResult::new(id, Status::Ok, "setup.firewall.off"),
        (false, false) => CheckResult::new(id, Status::Warning, "setup.firewall.missing").fix(Fix::AddFirewallRule),
        (tcp, _) => {
            let missing = if tcp { "UDP" } else { "TCP" };
            CheckResult::new(id, Status::Warning, "setup.firewall.partial").detail(missing).fix(Fix::AddFirewallRule)
        }
    }
}

// Command line given to `cmd.exe /c` (elevated) that allows `exe` on private networks for every
// protocol, TCP and UDP included. None when the path holds characters cmd.exe would interpret.
pub fn firewall_command(exe: &str) -> Option<String> {
    if exe.is_empty() || exe.chars().any(|c| matches!(c, '"' | '%' | '^' | '!' | '&' | '|' | '<' | '>' | '\r' | '\n')) {
        return None;
    }
    let name = RULE_NAME;
    Some(format!(
        "/d /s /c \"netsh advfirewall firewall delete rule name=\"{name}\" program=\"{exe}\" & netsh advfirewall firewall add rule name=\"{name}\" dir=in action=allow program=\"{exe}\" enable=yes profile=private protocol=any\""
    ))
}

const RULE_NAME: &str = "RustPlay (asistente)";

// ---------------------------------------------------------------- mDNS

// Programs known to run their own mDNS responder and to clash with other AirPlay receivers.
// Browsers, Spotify, Steam, Windows' own DNS client and the like also listen on UDP 5353, but they
// share the port and are no problem, so only these names are reported.
const KNOWN_RESPONDERS: [&str; 5] = ["mdnsresponder.exe", "itunes.exe", "applemobiledeviceservice.exe", "airserver.exe", "bonjour.exe"];

// File names of the known mDNS responders among the processes holding UDP 5353 (this process excluded).
pub fn foreign_port_holders(holders: &[(u32, String)], own_pid: u32) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (pid, name) in holders {
        if *pid == own_pid || !KNOWN_RESPONDERS.contains(&name.to_lowercase().as_str()) {
            continue;
        }
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name.clone());
        }
    }
    names
}

pub fn classify_mdns(registration: Result<(), String>, foreign: &[String]) -> CheckResult {
    let id = CheckId::Mdns;
    match registration {
        Err(e) => CheckResult::new(id, Status::Problem, "setup.mdns.failed").detail(e),
        Ok(()) if foreign.is_empty() => CheckResult::new(id, Status::Ok, "setup.mdns.ok"),
        Ok(()) => CheckResult::new(id, Status::Warning, "setup.mdns.shared").detail(foreign.join(", ")),
    }
}

// ---------------------------------------------------------------- GPU

pub struct GpuProbe {
    pub device: Result<(), String>,
    // Whether the H.264 decoder / encoder is a hardware one.
    pub decoder: Result<bool, String>,
    pub encoder: Result<bool, String>,
}

pub fn classify_gpu(probe: &GpuProbe) -> CheckResult {
    let id = CheckId::Gpu;
    if let Err(e) = &probe.device {
        return CheckResult::new(id, Status::Problem, "setup.gpu.no_device").detail(e.clone());
    }
    let (decoder, encoder) = (&probe.decoder, &probe.encoder);
    match (decoder, encoder) {
        (Err(e), _) => CheckResult::new(id, Status::Problem, "setup.gpu.no_decoder").detail(e.clone()),
        (_, Err(e)) => CheckResult::new(id, Status::Problem, "setup.gpu.no_encoder").detail(e.clone()),
        (Ok(true), Ok(true)) => CheckResult::new(id, Status::Ok, "setup.gpu.ok"),
        (Ok(false), Ok(false)) => CheckResult::new(id, Status::Warning, "setup.gpu.software_both"),
        (Ok(false), Ok(true)) => CheckResult::new(id, Status::Warning, "setup.gpu.software_decoder"),
        (Ok(true), Ok(false)) => CheckResult::new(id, Status::Warning, "setup.gpu.software_encoder"),
    }
}

// ---------------------------------------------------------------- audio

pub fn classify_audio(devices: Result<Vec<String>, String>) -> CheckResult {
    let id = CheckId::Audio;
    match devices {
        Err(e) => CheckResult::new(id, Status::Problem, "setup.audio.failed").detail(e),
        Ok(list) => match list.first() {
            None => CheckResult::new(id, Status::Warning, "setup.audio.none"),
            Some(name) => CheckResult::new(id, Status::Ok, "setup.audio.ok").detail(name.clone()),
        },
    }
}

// ---------------------------------------------------------------- receiver port

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortProbe {
    Free,
    InUse,
    Error(String),
}

pub fn classify_port(port: u16, probe: &PortProbe, receiver_running: bool) -> CheckResult {
    let id = CheckId::Port;
    match probe {
        PortProbe::Free => CheckResult::new(id, Status::Ok, "setup.port.free").detail(port.to_string()),
        PortProbe::InUse if receiver_running => CheckResult::new(id, Status::Ok, "setup.port.ours").detail(port.to_string()),
        PortProbe::InUse => CheckResult::new(id, Status::Problem, "setup.port.busy").detail(port.to_string()),
        PortProbe::Error(e) => CheckResult::new(id, Status::Warning, "setup.port.error").detail(e.clone()),
    }
}

// Bind test on the loopback interface only.
pub fn probe_port(port: u16) -> PortProbe {
    match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) {
        Ok(_) => PortProbe::Free,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse || e.kind() == std::io::ErrorKind::PermissionDenied => PortProbe::InUse,
        Err(e) => PortProbe::Error(e.to_string()),
    }
}

// ---------------------------------------------------------------- report

// Plain-text report for support. `describe` returns the localized title and explanation of a result.
pub fn report(version: &str, results: &[CheckResult], describe: impl Fn(&CheckResult) -> (String, String)) -> String {
    let mut out = format!("RustPlay {version} - Windows {}\r\n", windows_build());
    for result in results {
        let tag = match result.status {
            Status::Ok => "OK",
            Status::Warning => "WARNING",
            Status::Problem => "PROBLEM",
        };
        let (title, text) = describe(result);
        out.push_str(&format!("[{tag}] {title}: {text}\r\n"));
    }
    out
}

fn windows_build() -> String {
    use windows::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: the structure is initialised with its size.
    #[allow(deprecated)]
    let ok = unsafe { GetVersionExW(&mut info).is_ok() };
    if ok { format!("{}.{}.{}", info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber) } else { String::from("?") }
}

// ---------------------------------------------------------------- gathering the facts (Windows)

mod win {
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, WAIT_OBJECT_0};
    use windows::Win32::NetworkManagement::IpHelper::{GetExtendedUdpTable, MIB_UDPROW_OWNER_PID, UDP_TABLE_OWNER_PID};
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_PROFILE2_PRIVATE, NET_FW_RULE_DIR_IN, NetFwPolicy2,
    };
    use windows::Win32::Networking::NetworkListManager::{
        INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_PRIVATE, NLM_NETWORK_CATEGORY_PUBLIC, NetworkListManager,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch};
    use windows::Win32::System::Ole::IEnumVARIANT;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        WaitForSingleObject,
    };
    use windows::Win32::System::Variant::VariantClear;
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

    use super::{NetCategory, RuleInfo};

    // Initialises COM for the current worker thread (multithreaded) and undoes it on drop.
    pub struct Com(bool);

    impl Com {
        pub fn new() -> Self {
            // SAFETY: balanced by CoUninitialize in Drop when it succeeded.
            Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() })
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: matches the successful CoInitializeEx.
                unsafe { CoUninitialize() };
            }
        }
    }

    pub fn connected_networks() -> Result<Vec<(String, NetCategory)>, String> {
        // SAFETY: plain COM calls on objects owned here.
        unsafe {
            let manager: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
            let list = manager.GetNetworks(NLM_ENUM_NETWORK_CONNECTED).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            loop {
                let mut item = [None];
                let mut fetched = 0u32;
                if list.Next(&mut item, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(network) = item[0].take() else { break };
                let name = network.GetName().map(|n| n.to_string()).unwrap_or_default();
                let category = match network.GetCategory() {
                    Ok(c) if c == NLM_NETWORK_CATEGORY_PUBLIC => NetCategory::Public,
                    Ok(c) if c == NLM_NETWORK_CATEGORY_PRIVATE => NetCategory::Private,
                    Ok(_) => NetCategory::Domain,
                    Err(_) => continue,
                };
                out.push((name, category));
            }
            Ok(out)
        }
    }

    pub fn firewall_rules() -> Result<(Vec<RuleInfo>, bool), String> {
        // SAFETY: plain COM calls on objects owned here; every VARIANT is cleared after use.
        unsafe {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
            let private_enabled = policy.get_FirewallEnabled(NET_FW_PROFILE2_PRIVATE).map(|b| b.as_bool()).unwrap_or(true);
            let rules = policy.Rules().map_err(|e| e.to_string())?;
            let enumerator: IEnumVARIANT = rules._NewEnum().and_then(|u| u.cast()).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            loop {
                let mut item = [Default::default()];
                let mut fetched = 0u32;
                if enumerator.Next(&mut item, &mut fetched).0 != 0 || fetched == 0 {
                    break;
                }
                let dispatch: Option<IDispatch> = (*item[0].Anonymous.Anonymous).Anonymous.pdispVal.as_ref().cloned();
                let _ = VariantClear(&mut item[0]);
                let Some(rule) = dispatch.and_then(|d| d.cast::<INetFwRule>().ok()) else { continue };
                let app = rule.ApplicationName().map(|b| b.to_string()).unwrap_or_default();
                if app.is_empty() {
                    continue;
                }
                out.push(RuleInfo {
                    app,
                    enabled: rule.Enabled().map(|b| b.as_bool()).unwrap_or(false),
                    inbound: rule.Direction().is_ok_and(|d| d == NET_FW_RULE_DIR_IN),
                    allow: rule.Action().is_ok_and(|a| a == NET_FW_ACTION_ALLOW),
                    profiles: rule.Profiles().unwrap_or(0),
                    protocol: rule.Protocol().unwrap_or(256),
                });
            }
            Ok((out, private_enabled))
        }
    }

    // (pid, executable file name) of every process with an IPv4 UDP socket bound to `port`.
    pub fn udp_port_holders(port: u16) -> Vec<(u32, String)> {
        const AF_INET: u32 = 2;
        let mut size = 0u32;
        // SAFETY: the first call only asks for the size; the second fills a buffer of that size.
        unsafe {
            let rc = GetExtendedUdpTable(None, &mut size, false, AF_INET, UDP_TABLE_OWNER_PID, 0);
            if rc != ERROR_INSUFFICIENT_BUFFER.0 {
                return Vec::new();
            }
            let mut buffer = vec![0u64; (size as usize).div_ceil(8) + 8];
            let rc = GetExtendedUdpTable(Some(buffer.as_mut_ptr().cast()), &mut size, false, AF_INET, UDP_TABLE_OWNER_PID, 0);
            if rc != 0 {
                return Vec::new();
            }
            let count = *buffer.as_ptr().cast::<u32>() as usize;
            let rows = std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>().add(4).cast::<MIB_UDPROW_OWNER_PID>(), count);
            let mut out = Vec::new();
            for row in rows {
                if u16::from_be((row.dwLocalPort & 0xFFFF) as u16) == port {
                    out.push((row.dwOwningPid, process_name(row.dwOwningPid)));
                }
            }
            out
        }
    }

    fn process_name(pid: u32) -> String {
        // SAFETY: the handle is closed here; the buffer length is passed in and updated by the call.
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return format!("PID {pid}");
            };
            let mut buffer = [0u16; 520];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_FORMAT(0), PWSTR(buffer.as_mut_ptr()), &mut len).is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                return format!("PID {pid}");
            }
            let path = String::from_utf16_lossy(&buffer[..len as usize]);
            path.rsplit('\\').next().unwrap_or(&path).to_owned()
        }
    }

    pub fn own_pid() -> u32 {
        // SAFETY: no parameters.
        unsafe { GetCurrentProcessId() }
    }

    // Runs `exe parameters` elevated (UAC prompt) and waits for it; Err when the user declines.
    pub fn run_elevated(exe: &str, parameters: &str) -> Result<u32, String> {
        let verb = HSTRING::from("runas");
        let file = HSTRING::from(exe);
        let params = HSTRING::from(parameters);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: the strings outlive the call; the process handle is closed after waiting.
        unsafe {
            ShellExecuteExW(&mut info).map_err(|e| e.to_string())?;
            if info.hProcess.is_invalid() {
                return Ok(0);
            }
            let wait = WaitForSingleObject(info.hProcess, 60_000);
            let mut code = 0u32;
            let _ = windows::Win32::System::Threading::GetExitCodeProcess(info.hProcess, &mut code);
            let _ = CloseHandle(info.hProcess);
            if wait != WAIT_OBJECT_0 {
                return Err(String::from("timeout"));
            }
            Ok(code)
        }
    }

}

fn exe_string() -> Option<String> {
    std::env::current_exe().ok().map(|p| exe_path(&p))
}

fn check_network() -> CheckResult {
    let _com = win::Com::new();
    match win::connected_networks() {
        Ok(list) => classify_network(&list),
        Err(e) => CheckResult::new(CheckId::Network, Status::Warning, "setup.network.unknown").detail(e),
    }
}

fn check_firewall() -> CheckResult {
    let _com = win::Com::new();
    let Some(exe) = exe_string() else {
        return CheckResult::new(CheckId::Firewall, Status::Warning, "setup.firewall.unknown");
    };
    match win::firewall_rules() {
        Ok((rules, private_enabled)) => classify_firewall(&rules, &exe, private_enabled),
        Err(e) => CheckResult::new(CheckId::Firewall, Status::Warning, "setup.firewall.unknown").detail(e),
    }
}

fn check_mdns() -> CheckResult {
    let registration = (|| -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        let spec = rustplay_core::mdns::ServiceSpec {
            instance: format!("RustPlay check {}", std::process::id()),
            service_type: "_rustplaycheck._tcp",
            port: 7000,
            txt: vec![(String::from("check"), String::from("1"))],
        };
        // Dropping the registration deregisters the service again.
        runtime.block_on(rustplay_core::mdns::register(spec)).map(drop).map_err(|e| e.to_string())
    })();
    let holders = win::udp_port_holders(5353);
    classify_mdns(registration, &foreign_port_holders(&holders, win::own_pid()))
}

fn check_gpu() -> CheckResult {
    use rustplay_core::media::gpu::GpuDevice;
    use rustplay_core::media::video::{EncoderConfig, H264Decoder, H264Encoder};
    let gpu = match GpuDevice::create() {
        Ok(gpu) => gpu,
        Err(e) => {
            return classify_gpu(&GpuProbe { device: Err(e.to_string()), decoder: Err(String::new()), encoder: Err(String::new()) });
        }
    };
    let decoder = H264Decoder::new(gpu.clone()).map(|d| d.is_hardware()).map_err(|e| e.to_string());
    let encoder = H264Encoder::new(gpu, EncoderConfig { width: 1280, height: 720, fps: 30, bitrate: 2_000_000 })
        .map(|e| e.is_hardware())
        .map_err(|e| e.to_string());
    classify_gpu(&GpuProbe { device: Ok(()), decoder, encoder })
}

fn check_audio() -> CheckResult {
    let devices = rustplay_core::media::audio::render_devices()
        .map(|list| list.into_iter().map(|d| d.name).collect())
        .map_err(|e| e.to_string());
    classify_audio(devices)
}

fn check_port(port: u16, receiver_running: bool) -> CheckResult {
    classify_port(port, &probe_port(port), receiver_running)
}

// Runs every check, in parallel, and returns the results in `CheckId::ALL` order.
pub fn run_all(port: u16, receiver_running: bool) -> Vec<CheckResult> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = CheckId::ALL
            .into_iter()
            .map(|id| {
                scope.spawn(move || match id {
                    CheckId::Network => check_network(),
                    CheckId::Firewall => check_firewall(),
                    CheckId::Mdns => check_mdns(),
                    CheckId::Gpu => check_gpu(),
                    CheckId::Audio => check_audio(),
                    CheckId::Port => check_port(port, receiver_running),
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(CheckId::ALL)
            .map(|(handle, id)| {
                handle.join().unwrap_or_else(|_| CheckResult::new(id, Status::Warning, "setup.crashed"))
            })
            .collect()
    })
}

// Fake findings for the design preview.
pub fn sample_results() -> Vec<CheckResult> {
    vec![
        CheckResult::new(CheckId::Network, Status::Warning, "setup.network.public").detail("Red doméstica").fix(Fix::OpenNetworkSettings),
        CheckResult::new(CheckId::Firewall, Status::Warning, "setup.firewall.missing").fix(Fix::AddFirewallRule),
        CheckResult::new(CheckId::Mdns, Status::Ok, "setup.mdns.ok"),
        CheckResult::new(CheckId::Gpu, Status::Warning, "setup.gpu.software_encoder"),
        CheckResult::new(CheckId::Audio, Status::Ok, "setup.audio.ok").detail("Altavoces (Realtek Audio)"),
        CheckResult::new(CheckId::Port, Status::Problem, "setup.port.busy").detail("7000"),
    ]
}

pub fn open_network_settings() {
    crate::links::open("ms-settings:network-status");
}

pub fn open_firewall_console() {
    crate::links::open("wf.msc");
}

// Adds the inbound allow rule for this executable. Elevated: Windows shows the UAC prompt, and it
// is only ever called from the button the user clicked.
pub fn add_firewall_rule() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe_path(&exe);
    let command = firewall_command(&exe).ok_or_else(|| String::from("ruta no válida"))?;
    let system = std::env::var("SystemRoot").unwrap_or_else(|_| String::from(r"C:\Windows"));
    match win::run_elevated(&format!(r"{system}\System32\cmd.exe"), &command)? {
        0 => Ok(()),
        code => Err(format!("netsh terminó con el código {code}")),
    }
}

fn exe_path(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(app: &str, allow: bool, profiles: i32, protocol: i32) -> RuleInfo {
        RuleInfo { app: app.into(), enabled: true, inbound: true, allow, profiles, protocol }
    }

    const EXE: &str = r"C:\Program Files\RustPlay\RustPlay.exe";

    #[test]
    fn public_network_is_a_warning_with_a_fix() {
        let r = classify_network(&[("Casa".into(), NetCategory::Private), ("Café".into(), NetCategory::Public)]);
        assert_eq!(r.status, Status::Warning);
        assert_eq!(r.detail, "Café");
        assert_eq!(r.fix, Some(Fix::OpenNetworkSettings));
        let ok = classify_network(&[("Casa".into(), NetCategory::Private), ("Oficina".into(), NetCategory::Domain)]);
        assert_eq!(ok.status, Status::Ok);
        assert_eq!(classify_network(&[]).message, "setup.network.none");
    }

    #[test]
    fn firewall_rule_matching() {
        let any = rule(&EXE.to_lowercase(), true, 0x7FFF_FFFF, PROTO_ANY);
        assert_eq!(classify_firewall(&[any], EXE, true).status, Status::Ok);
        // Two rules, one per protocol, as Windows creates from its own prompt.
        let pair = [rule(EXE, true, PROFILE_PRIVATE, PROTO_TCP), rule(EXE, true, PROFILE_PRIVATE, PROTO_UDP)];
        assert_eq!(classify_firewall(&pair, EXE, true).status, Status::Ok);
        let tcp_only = [rule(EXE, true, PROFILE_PRIVATE, PROTO_TCP)];
        let r = classify_firewall(&tcp_only, EXE, true);
        assert_eq!((r.status, r.detail.as_str(), r.fix), (Status::Warning, "UDP", Some(Fix::AddFirewallRule)));
        // A public-only rule does not cover private networks.
        let public = [rule(EXE, true, 4, PROTO_ANY)];
        assert_eq!(classify_firewall(&public, EXE, true).message, "setup.firewall.missing");
        // Another program's rule does not count.
        let other = [rule(r"C:\other.exe", true, PROFILE_PRIVATE, PROTO_ANY)];
        assert_eq!(classify_firewall(&other, EXE, true).status, Status::Warning);
        // Disabled rules are ignored.
        let mut disabled = rule(EXE, true, PROFILE_PRIVATE, PROTO_ANY);
        disabled.enabled = false;
        assert_eq!(classify_firewall(&[disabled], EXE, true).status, Status::Warning);
        // Firewall off for private networks: nothing blocks.
        assert_eq!(classify_firewall(&[], EXE, false).message, "setup.firewall.off");
    }

    #[test]
    fn blocking_rule_is_a_problem() {
        let rules = [rule(EXE, true, PROFILE_PRIVATE, PROTO_ANY), rule(EXE, false, 0x7FFF_FFFF, PROTO_UDP)];
        let r = classify_firewall(&rules, EXE, true);
        assert_eq!((r.status, r.fix), (Status::Problem, Some(Fix::OpenFirewall)));
    }

    #[test]
    fn firewall_command_quotes_the_path_and_refuses_shell_characters() {
        let c = firewall_command(EXE).unwrap();
        assert!(c.starts_with("/d /s /c \"netsh advfirewall firewall delete rule"));
        assert!(c.contains(&format!("program=\"{EXE}\"")));
        assert!(c.contains("dir=in action=allow") && c.contains("profile=private") && c.contains("protocol=any"));
        assert!(c.ends_with('"'));
        assert_eq!(firewall_command(r"C:\a&b\x.exe"), None);
        assert_eq!(firewall_command(r"C:\%TEMP%\x.exe"), None);
        assert_eq!(firewall_command(""), None);
    }

    #[test]
    fn mdns_findings() {
        let holders = vec![(4, "svchost.exe".to_owned()), (5, "msedge.exe".to_owned()), (10, "mDNSResponder.exe".to_owned()), (11, "mdnsresponder.exe".to_owned()), (99, "RustPlay.exe".to_owned())];
        let foreign = foreign_port_holders(&holders, 99);
        assert_eq!(foreign, vec!["mDNSResponder.exe".to_owned()]);
        assert_eq!(classify_mdns(Ok(()), &foreign).status, Status::Warning);
        assert_eq!(classify_mdns(Ok(()), &[]).status, Status::Ok);
        let failed = classify_mdns(Err("x".into()), &[]);
        assert_eq!((failed.status, failed.detail.as_str()), (Status::Problem, "x"));
    }

    #[test]
    fn gpu_findings() {
        let probe = |decoder: Result<bool, String>, encoder: Result<bool, String>| GpuProbe { device: Ok(()), decoder, encoder };
        assert_eq!(classify_gpu(&probe(Ok(true), Ok(true))).status, Status::Ok);
        assert_eq!(classify_gpu(&probe(Ok(true), Ok(false))).message, "setup.gpu.software_encoder");
        assert_eq!(classify_gpu(&probe(Ok(false), Ok(true))).message, "setup.gpu.software_decoder");
        assert_eq!(classify_gpu(&probe(Ok(false), Ok(false))).status, Status::Warning);
        assert_eq!(classify_gpu(&probe(Err("e".into()), Ok(true))).status, Status::Problem);
        let none = GpuProbe { device: Err("sin GPU".into()), decoder: Ok(true), encoder: Ok(true) };
        assert_eq!(classify_gpu(&none).message, "setup.gpu.no_device");
    }

    #[test]
    fn audio_and_port_findings() {
        assert_eq!(classify_audio(Ok(vec!["Altavoces".into()])).detail, "Altavoces");
        assert_eq!(classify_audio(Ok(vec![])).status, Status::Warning);
        assert_eq!(classify_audio(Err("e".into())).status, Status::Problem);
        assert_eq!(classify_port(7000, &PortProbe::Free, false).status, Status::Ok);
        assert_eq!(classify_port(7000, &PortProbe::InUse, true).message, "setup.port.ours");
        assert_eq!(classify_port(7000, &PortProbe::InUse, false).status, Status::Problem);
    }

    // Runs the real checks on this PC (COM, firewall, GPU...); read-only apart from a short mDNS registration.
    #[test]
    #[ignore = "consulta el sistema real; ejecutar a mano"]
    fn live_system_check() {
        for r in run_all(7000, false) {
            println!("{:?} {:?} {} | {}", r.id, r.status, r.message, r.detail);
        }
    }

    #[test]
    fn port_probe_sees_a_busy_loopback_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(probe_port(port), PortProbe::InUse);
        drop(listener);
        assert_eq!(probe_port(port), PortProbe::Free);
    }

    #[test]
    fn overall_is_the_worst_status_and_the_report_lists_everything() {
        let results = sample_results();
        assert_eq!(overall(&results), Status::Problem);
        assert_eq!(overall(&results[2..3]), Status::Ok);
        assert_eq!(overall(&results[..2]), Status::Warning);
        assert_eq!(count(&results, Status::Warning), 3);
        let text = report("1.2.3", &results, |r| (format!("{:?}", r.id), r.message.to_owned()));
        assert!(text.starts_with("RustPlay 1.2.3 - Windows "));
        assert!(text.contains("[WARNING] Network: setup.network.public\r\n"));
        assert!(text.contains("[PROBLEM] Port: setup.port.busy\r\n"));
        assert_eq!(text.matches("\r\n").count(), 1 + results.len());
    }
}
