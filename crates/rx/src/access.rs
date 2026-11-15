// Who may connect to the receiver: optional PIN pairing, allowed and blocked devices, and
// asking the user for each new device.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustplay_core::identity::{PairingRecord, PairingStore};
use tokio::sync::oneshot;

// How long a connection waits for the user to answer an access request.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30);
// A device refused without "remember" is not asked about again for this long.
const DENY_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccessPolicy {
    #[default]
    Everyone,
    AllowedOnly,
    Ask,
}

// Live options: device ids are compared without case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessConfig {
    pub policy: AccessPolicy,
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
}

// PIN pairing (announced over mDNS, so changing it needs a restart). With `fixed: None` a new
// random code is shown for every pairing attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinConfig {
    pub required: bool,
    pub fixed: Option<String>,
}

impl PinConfig {
    pub fn code(&self) -> String {
        self.fixed
            .as_deref()
            .map(str::trim)
            .filter(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_digit()))
            .map_or_else(rustplay_core::crypto::pin::random_pin, str::to_owned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

pub fn normalize(device_id: &str) -> String {
    device_id.trim().to_ascii_uppercase()
}

// `remembered` is an earlier answer for this device during this run (see `AccessState`).
pub fn decide(config: &AccessConfig, device_id: &str, remembered: Option<bool>) -> Decision {
    let id = normalize(device_id);
    let listed = |list: &[String]| !id.is_empty() && list.iter().any(|d| normalize(d) == id);
    if listed(&config.blocked) {
        return Decision::Deny;
    }
    if listed(&config.allowed) {
        return Decision::Allow;
    }
    match (config.policy, remembered) {
        (AccessPolicy::Everyone, _) => Decision::Allow,
        (AccessPolicy::AllowedOnly, _) => Decision::Deny,
        (AccessPolicy::Ask, Some(true)) => Decision::Allow,
        (AccessPolicy::Ask, Some(false)) => Decision::Deny,
        (AccessPolicy::Ask, None) => Decision::Ask,
    }
}

struct Pending {
    key: String,
    waiters: Vec<oneshot::Sender<bool>>,
}

pub(crate) struct Ticket {
    pub id: u64,
    // False when another connection of the same device is already waiting for this answer.
    pub first: bool,
    pub answer: oneshot::Receiver<bool>,
}

pub(crate) struct AccessState {
    pub pin: PinConfig,
    clients: Option<PathBuf>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    answered: Mutex<HashMap<String, (bool, Instant)>>,
    // Code shown to each address. iOS asks for it on one connection (/pair-pin-start), closes it
    // while the user types, and sends the code on a new connection (/pair-setup-pin).
    codes: Mutex<HashMap<std::net::IpAddr, (String, Instant, u8)>>,
}

// Wrong attempts allowed for one code. iOS itself makes the first one (before it asks the user),
// so a single failure must not discard the code that is on screen.
const CODE_ATTEMPTS: u8 = 5;
// How long a shown code stays valid for the device's next connection.
const CODE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(180);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AccessState {
    // `clients` is the store of devices paired with a PIN (`None`: nothing is remembered).
    pub fn new(pin: PinConfig, clients: Option<PathBuf>) -> Self {
        Self {
            pin,
            clients,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            answered: Mutex::new(HashMap::new()),
            codes: Mutex::new(HashMap::new()),
        }
    }

    // The code for `ip`: the one already shown if still valid, else a new one. The flag is
    // true when the code is new (so it must be shown).
    pub fn issue_code(&self, ip: std::net::IpAddr) -> (String, bool) {
        let mut codes = lock(&self.codes);
        codes.retain(|_, (_, at, _)| at.elapsed() < CODE_LIFETIME);
        if let Some((code, _, _)) = codes.get(&ip) {
            return (code.clone(), false);
        }
        let code = self.pin.code();
        codes.insert(ip, (code.clone(), Instant::now(), 0));
        (code, true)
    }

    // Counts a wrong code; true once the attempts run out and the code is discarded.
    pub fn code_failed(&self, ip: std::net::IpAddr) -> bool {
        let mut codes = lock(&self.codes);
        let Some(entry) = codes.get_mut(&ip) else { return true };
        entry.2 += 1;
        if entry.2 >= CODE_ATTEMPTS {
            codes.remove(&ip);
            return true;
        }
        false
    }

    pub fn clear_code(&self, ip: std::net::IpAddr) {
        lock(&self.codes).remove(&ip);
    }

    pub fn remembered(&self, key: &str) -> Option<bool> {
        let mut answered = lock(&self.answered);
        match answered.get(key) {
            Some((false, at)) if at.elapsed() >= DENY_COOLDOWN => {
                answered.remove(key);
                None
            }
            Some((allow, _)) => Some(*allow),
            None => None,
        }
    }

    pub fn enqueue(&self, key: &str) -> Ticket {
        let (tx, answer) = oneshot::channel();
        let mut pending = lock(&self.pending);
        if let Some((&id, entry)) = pending.iter_mut().find(|(_, p)| p.key == key) {
            entry.waiters.push(tx);
            return Ticket { id, first: false, answer };
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        pending.insert(id, Pending { key: key.to_owned(), waiters: vec![tx] });
        Ticket { id, first: true, answer }
    }

    // Returns false when the request already timed out or was answered.
    pub fn answer(&self, id: u64, allow: bool) -> bool {
        let Some(entry) = lock(&self.pending).remove(&id) else {
            return false;
        };
        lock(&self.answered).insert(entry.key, (allow, Instant::now()));
        for waiter in entry.waiters {
            let _ = waiter.send(allow);
        }
        true
    }

    // Drops a request nobody answered; every connection waiting on it is refused.
    pub fn abandon(&self, id: u64) -> bool {
        lock(&self.pending).remove(&id).is_some()
    }

    fn store(&self) -> Option<PairingStore> {
        let path = self.clients.clone()?;
        match PairingStore::load_from(path) {
            Ok(store) => Some(store),
            Err(e) => {
                tracing::warn!("no se pudo leer la lista de dispositivos emparejados: {e}");
                None
            }
        }
    }

    pub fn is_paired(&self, public_key: &[u8; 32]) -> bool {
        self.store().is_some_and(|s| s.find_key(public_key).is_some())
    }

    pub fn remember_pairing(&self, device_id: &str, public_key: [u8; 32]) {
        let Some(mut store) = self.store() else { return };
        let record = PairingRecord {
            peer_id: normalize(device_id),
            peer_public_key: public_key,
        };
        if let Err(e) = store.upsert(record) {
            tracing::warn!("no se pudo guardar el emparejamiento de {device_id}: {e}");
        }
    }
}

// Removes the PIN pairing of a device, so it has to enter a code again. Works with the receiver off.
pub fn forget_paired_device(device_id: &str) -> rustplay_core::Result<bool> {
    let mut store = PairingStore::load_clients()?;
    store.remove(&normalize(device_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(policy: AccessPolicy) -> AccessConfig {
        AccessConfig {
            policy,
            allowed: vec!["aa:bb:cc:dd:ee:01".into()],
            blocked: vec!["AA:BB:CC:DD:EE:02".into()],
        }
    }

    const ALLOWED: &str = "AA:BB:CC:DD:EE:01";
    const BLOCKED: &str = "aa:bb:cc:dd:ee:02";
    const OTHER: &str = "AA:BB:CC:DD:EE:03";

    #[test]
    fn blocked_devices_are_always_refused() {
        for policy in [AccessPolicy::Everyone, AccessPolicy::AllowedOnly, AccessPolicy::Ask] {
            assert_eq!(decide(&config(policy), BLOCKED, Some(true)), Decision::Deny, "{policy:?}");
        }
    }

    #[test]
    fn allowed_devices_always_pass() {
        for policy in [AccessPolicy::Everyone, AccessPolicy::AllowedOnly, AccessPolicy::Ask] {
            assert_eq!(decide(&config(policy), ALLOWED, Some(false)), Decision::Allow, "{policy:?}");
        }
    }

    #[test]
    fn unknown_devices_follow_the_policy() {
        assert_eq!(decide(&config(AccessPolicy::Everyone), OTHER, None), Decision::Allow);
        assert_eq!(decide(&config(AccessPolicy::AllowedOnly), OTHER, None), Decision::Deny);
        assert_eq!(decide(&config(AccessPolicy::Ask), OTHER, None), Decision::Ask);
        assert_eq!(decide(&config(AccessPolicy::Ask), OTHER, Some(true)), Decision::Allow);
        assert_eq!(decide(&config(AccessPolicy::Ask), OTHER, Some(false)), Decision::Deny);
        assert_eq!(decide(&config(AccessPolicy::AllowedOnly), "", None), Decision::Deny);
        assert_eq!(decide(&config(AccessPolicy::Ask), " ", None), Decision::Ask);
    }

    #[test]
    fn answers_reach_every_waiting_connection_and_are_remembered() {
        let state = AccessState::new(PinConfig::default(), None);
        let first = state.enqueue("A");
        let second = state.enqueue("A");
        let other = state.enqueue("B");
        assert!(first.first && !second.first && other.first);
        assert_eq!(first.id, second.id);
        assert_ne!(first.id, other.id);
        assert!(state.answer(first.id, true));
        assert!(!state.answer(first.id, false));
        let (mut a, mut b) = (first.answer, second.answer);
        assert_eq!(a.try_recv(), Ok(true));
        assert_eq!(b.try_recv(), Ok(true));
        assert_eq!(state.remembered("A"), Some(true));
        assert_eq!(state.remembered("B"), None);
        assert!(state.abandon(other.id));
        let mut c = other.answer;
        assert!(c.try_recv().is_err());
        assert!(state.answer(state.enqueue("B").id, false));
        assert_eq!(state.remembered("B"), Some(false));
    }

    #[test]
    fn fixed_pin_is_used_only_when_valid() {
        let fixed = |f: &str| PinConfig { required: true, fixed: Some(f.into()) }.code();
        assert_eq!(fixed("0420"), "0420");
        assert_eq!(fixed(" 1234 "), "1234");
        for bad in ["12", "12a4", "12345"] {
            let code = fixed(bad);
            assert_eq!(code.len(), 4);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn pairings_are_stored_and_found_by_key() {
        let dir = std::env::temp_dir().join(format!("rustplay-access-{}", std::process::id()));
        let path = dir.join("client-pairings.plist");
        let state = AccessState::new(PinConfig::default(), Some(path.clone()));
        assert!(!state.is_paired(&[5; 32]));
        state.remember_pairing("aa:bb", [5; 32]);
        assert!(state.is_paired(&[5; 32]));
        let store = PairingStore::load_from(path).unwrap();
        assert_eq!(store.find("AA:BB").unwrap().peer_public_key, [5; 32]);
        std::fs::remove_dir_all(dir).ok();
        assert!(!AccessState::new(PinConfig::default(), None).is_paired(&[5; 32]));
    }
}
