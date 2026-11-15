// Receiver-wide bookkeeping of senders: which sessions are streaming (to give the audio to the
// newest one and, in single-device mode, to replace the previous device), and which mirror
// windows exist, so a device that reconnects gets its window back.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

// How long a mirror window stays open, showing "connection paused", after its sender went away
// without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepWindow {
    Off,
    For(Duration),
    Always,
}

impl Default for KeepWindow {
    fn default() -> Self {
        Self::For(Duration::from_secs(5 * 60))
    }
}

impl KeepWindow {
    pub fn enabled(self) -> bool {
        self != Self::Off
    }

    // True once a window parked for `parked_for` must close.
    pub fn expired(self, parked_for: Duration) -> bool {
        match self {
            Self::Off => true,
            Self::For(limit) => parked_for >= limit,
            Self::Always => false,
        }
    }

    // How long the window may still wait, capped at `cap` (None = forever).
    pub fn wait(self, parked_for: Duration, cap: Duration) -> Duration {
        match self {
            Self::Off => Duration::ZERO,
            Self::For(limit) => limit.saturating_sub(parked_for).min(cap),
            Self::Always => cap,
        }
    }
}

// Identifies a sender across reconnections: its AirPlay deviceID when both sides know it,
// otherwise its IP address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceKey {
    pub id: Option<String>,
    pub ip: IpAddr,
}

impl DeviceKey {
    pub fn new(id: Option<&str>, ip: IpAddr) -> Self {
        let id = id.map(str::trim).filter(|id| !id.is_empty()).map(str::to_ascii_uppercase);
        Self { id, ip }
    }

    pub fn same_device(&self, other: &Self) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.ip == other.ip,
        }
    }
}

impl std::fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.id {
            Some(id) => write!(f, "{} ({id})", self.ip),
            None => write!(f, "{}", self.ip),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct SessionSlot {
    id: u64,
    key: DeviceKey,
    connection: CancellationToken,
}

#[derive(Default)]
struct SessionsInner {
    next: u64,
    sessions: Vec<SessionSlot>,
    // Sessions playing audio, oldest first; only the last one is heard.
    audio: Vec<(u64, Arc<AtomicBool>)>,
}

#[derive(Default)]
pub struct Sessions {
    inner: Mutex<SessionsInner>,
}

impl Sessions {
    // Registers a session that starts streaming. With `exclusive` (one device at a time), the
    // sessions of every other device are disconnected; their keys are returned.
    pub fn join(&self, key: DeviceKey, connection: CancellationToken, exclusive: bool) -> (u64, Vec<DeviceKey>) {
        let mut inner = lock(&self.inner);
        let mut replaced = Vec::new();
        if exclusive {
            for other in inner.sessions.iter().filter(|s| !s.key.same_device(&key)) {
                other.connection.cancel();
                replaced.push(other.key.clone());
            }
        }
        inner.next += 1;
        let id = inner.next;
        inner.sessions.push(SessionSlot { id, key, connection });
        (id, replaced)
    }

    pub fn leave(&self, id: u64) {
        let mut inner = lock(&self.inner);
        inner.sessions.retain(|s| s.id != id);
        drop(inner);
        self.audio_stopped(id);
    }

    #[cfg(test)]
    pub fn count(&self) -> usize {
        lock(&self.inner).sessions.len()
    }

    // The newest audio stream is heard; the others are muted until it stops.
    pub fn audio_started(&self, id: u64, muted: Arc<AtomicBool>) {
        let mut inner = lock(&self.inner);
        inner.audio.retain(|(other, _)| *other != id);
        for (_, flag) in &inner.audio {
            flag.store(true, Ordering::Release);
        }
        muted.store(false, Ordering::Release);
        inner.audio.push((id, muted));
    }

    pub fn audio_stopped(&self, id: u64) {
        let mut inner = lock(&self.inner);
        let before = inner.audio.len();
        inner.audio.retain(|(other, _)| *other != id);
        if inner.audio.len() != before
            && let Some((_, flag)) = inner.audio.last()
        {
            flag.store(false, Ordering::Release);
        }
    }
}

pub enum Control<A> {
    // A new session of the same device takes over the window.
    Attach(A),
    Close,
}

type Detach = Box<dyn Fn() + Send>;

struct WindowSlot<A> {
    id: u64,
    key: DeviceKey,
    // Sessions handed over by `claim` and not yet taken by the window's thread.
    pending: usize,
    // Its sender went away; the window waits for it to come back.
    parked: bool,
    control: Sender<Control<A>>,
    thread: Option<JoinHandle<()>>,
    // Lets go of the current session when the same device starts a new one.
    detach: Option<Detach>,
}

// The mirror windows, each driven by its own thread. A window's thread only ends after
// `retire` agrees, so a session handed over by `claim` is never lost.
pub struct Windows<A> {
    inner: Mutex<(u64, Vec<WindowSlot<A>>)>,
}

impl<A> Default for Windows<A> {
    fn default() -> Self {
        Self { inner: Mutex::new((0, Vec::new())) }
    }
}

impl<A> Windows<A> {
    // Registers a new active window and returns its id.
    pub fn insert(&self, key: DeviceKey, control: Sender<Control<A>>) -> u64 {
        let mut inner = lock(&self.inner);
        inner.0 += 1;
        let id = inner.0;
        inner.1.push(WindowSlot { id, key, pending: 0, parked: false, control, thread: None, detach: None });
        id
    }

    pub fn set_thread(&self, id: u64, thread: JoinHandle<()>) {
        if let Some(slot) = lock(&self.inner).1.iter_mut().find(|s| s.id == id) {
            slot.thread = Some(thread);
        }
    }

    // Forgets a window whose thread could not start or gave up.
    pub fn remove(&self, id: u64) {
        lock(&self.inner).1.retain(|s| s.id != id);
    }

    // Hands `payload` to the window of the same device, if there is one: a parked one, or an
    // active one whose (stale) session is let go. Gives the payload back when there is none.
    pub fn claim(&self, key: &DeviceKey, payload: A) -> Result<u64, A> {
        let mut inner = lock(&self.inner);
        let mut payload = payload;
        loop {
            let found = inner.1.iter().position(|s| s.pending == 0 && s.key.same_device(key));
            let Some(index) = found else { return Err(payload) };
            let slot = &mut inner.1[index];
            match slot.control.send(Control::Attach(payload)) {
                Ok(()) => {
                    if let Some(detach) = slot.detach.take() {
                        detach();
                    }
                    slot.pending += 1;
                    slot.key = key.clone();
                    return Ok(slot.id);
                }
                Err(e) => {
                    // The thread is gone; forget the window and look for another one.
                    let Control::Attach(p) = e.0 else { unreachable!() };
                    payload = p;
                    inner.1.remove(index);
                }
            }
        }
    }

    // The window's thread took a session: its first one, or (`claimed`) one handed over.
    pub fn activate(&self, id: u64, detach: Detach, claimed: bool) {
        if let Some(slot) = lock(&self.inner).1.iter_mut().find(|s| s.id == id) {
            if claimed {
                slot.pending = slot.pending.saturating_sub(1);
            }
            slot.parked = false;
            slot.detach = Some(detach);
        }
    }

    // The window's session ended without the user asking; the window waits for its device.
    // False when a new session is already on its way.
    pub fn park(&self, id: u64) -> bool {
        match lock(&self.inner).1.iter_mut().find(|s| s.id == id) {
            Some(slot) if slot.pending > 0 => false,
            Some(slot) => {
                slot.parked = true;
                slot.detach = None;
                true
            }
            None => true,
        }
    }

    // The window's thread wants to end. False when a new session was just handed over and is
    // waiting in the control channel; the thread must take it instead.
    pub fn retire(&self, id: u64) -> bool {
        let mut inner = lock(&self.inner);
        match inner.1.iter().position(|s| s.id == id) {
            Some(index) if inner.1[index].pending > 0 => false,
            Some(index) => {
                inner.1.remove(index);
                true
            }
            None => true,
        }
    }

    // Closes the waiting windows of every device but `key` (single-device mode).
    pub fn close_parked_except(&self, key: &DeviceKey) -> usize {
        let inner = lock(&self.inner);
        let mut closed = 0;
        for slot in inner.1.iter().filter(|s| s.parked && s.pending == 0 && !s.key.same_device(key)) {
            if slot.control.send(Control::Close).is_ok() {
                closed += 1;
            }
        }
        closed
    }

    #[cfg(test)]
    pub fn parked(&self) -> usize {
        lock(&self.inner).1.iter().filter(|s| s.parked && s.pending == 0).count()
    }

    // Asks every window to close and returns their threads, to be joined.
    pub fn close_all(&self) -> Vec<JoinHandle<()>> {
        let mut inner = lock(&self.inner);
        inner
            .1
            .iter_mut()
            .filter_map(|slot| {
                let _ = slot.control.send(Control::Close);
                slot.thread.take()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn key(id: Option<&str>, ip: [u8; 4]) -> DeviceKey {
        DeviceKey::new(id, IpAddr::from(ip))
    }

    #[test]
    fn keep_window_timeouts() {
        let five = KeepWindow::default();
        assert_eq!(five, KeepWindow::For(Duration::from_secs(300)));
        assert!(!five.expired(Duration::from_secs(299)));
        assert!(five.expired(Duration::from_secs(300)));
        assert!(KeepWindow::Off.expired(Duration::ZERO));
        assert!(!KeepWindow::Off.enabled());
        assert!(!KeepWindow::Always.expired(Duration::from_secs(1 << 30)));
        let cap = Duration::from_millis(50);
        assert_eq!(five.wait(Duration::from_secs(10), cap), cap);
        assert_eq!(five.wait(Duration::from_millis(299_980), cap), Duration::from_millis(20));
        assert_eq!(five.wait(Duration::from_secs(400), cap), Duration::ZERO);
        assert_eq!(KeepWindow::Always.wait(Duration::from_secs(400), cap), cap);
        assert_eq!(KeepWindow::Off.wait(Duration::ZERO, cap), Duration::ZERO);
    }

    #[test]
    fn devices_match_by_id_or_else_by_ip() {
        let a = key(Some("aa:bb:cc:dd:ee:ff"), [192, 168, 1, 10]);
        assert!(a.same_device(&key(Some("AA:BB:CC:DD:EE:FF"), [192, 168, 1, 99])), "same id, new IP");
        assert!(!a.same_device(&key(Some("11:22:33:44:55:66"), [192, 168, 1, 10])), "other id, same IP");
        assert!(a.same_device(&key(None, [192, 168, 1, 10])));
        assert!(!a.same_device(&key(None, [192, 168, 1, 11])));
        assert!(key(Some(" "), [10, 0, 0, 1]).id.is_none());
    }

    #[test]
    fn newest_audio_is_heard_and_focus_returns_when_it_stops() {
        let sessions = Sessions::default();
        let (a, _) = sessions.join(key(None, [10, 0, 0, 1]), CancellationToken::new(), false);
        let (b, _) = sessions.join(key(None, [10, 0, 0, 2]), CancellationToken::new(), false);
        let (ma, mb) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)));
        sessions.audio_started(a, Arc::clone(&ma));
        assert!(!ma.load(Ordering::Acquire));
        sessions.audio_started(b, Arc::clone(&mb));
        assert!(ma.load(Ordering::Acquire) && !mb.load(Ordering::Acquire));
        sessions.audio_stopped(b);
        assert!(!ma.load(Ordering::Acquire), "the older stream is heard again");
        sessions.audio_started(b, Arc::clone(&mb));
        sessions.leave(b);
        assert!(!ma.load(Ordering::Acquire));
        assert_eq!(sessions.count(), 1);
        // Stopping a muted stream does not change who is heard.
        sessions.audio_started(b, Arc::clone(&mb));
        sessions.audio_stopped(a);
        assert!(!mb.load(Ordering::Acquire));
    }

    #[test]
    fn single_device_mode_disconnects_other_devices_only() {
        let sessions = Sessions::default();
        let first = CancellationToken::new();
        let same = CancellationToken::new();
        sessions.join(key(Some("A"), [10, 0, 0, 1]), first.clone(), true);
        let (_, replaced) = sessions.join(key(Some("A"), [10, 0, 0, 1]), same.clone(), true);
        assert!(replaced.is_empty() && !first.is_cancelled());
        let (_, replaced) = sessions.join(key(Some("B"), [10, 0, 0, 2]), CancellationToken::new(), true);
        assert_eq!(replaced.len(), 2);
        assert!(first.is_cancelled() && same.is_cancelled());
        let other = CancellationToken::new();
        sessions.join(key(Some("C"), [10, 0, 0, 3]), other.clone(), false);
        assert!(!other.is_cancelled(), "several devices at once");
    }

    #[test]
    fn parked_window_is_claimed_by_the_same_device() {
        let windows = Windows::<u32>::default();
        let (tx, rx) = mpsc::channel();
        let id = windows.insert(key(Some("A"), [10, 0, 0, 1]), tx);
        assert!(windows.park(id));
        assert_eq!(windows.parked(), 1);
        assert_eq!(windows.claim(&key(Some("B"), [10, 0, 0, 1]), 7), Err(7), "another device");
        assert_eq!(windows.claim(&key(Some("A"), [10, 0, 0, 5]), 8), Ok(id));
        assert!(matches!(rx.try_recv(), Ok(Control::Attach(8))));
        assert!(!windows.retire(id), "a claimed window must take its session");
        assert_eq!(windows.claim(&key(Some("A"), [10, 0, 0, 5]), 9), Err(9), "already claimed");
        windows.activate(id, Box::new(|| {}), true);
        assert!(windows.retire(id));
        assert_eq!(windows.claim(&key(Some("A"), [10, 0, 0, 5]), 10), Err(10));
    }

    #[test]
    fn claiming_an_active_window_detaches_its_old_session() {
        let windows = Windows::<u32>::default();
        let (tx, rx) = mpsc::channel();
        let id = windows.insert(key(None, [10, 0, 0, 1]), tx);
        let detached = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&detached);
        windows.activate(id, Box::new(move || flag.store(true, Ordering::Release)), false);
        assert_eq!(windows.claim(&key(None, [10, 0, 0, 1]), 1), Ok(id));
        assert!(detached.load(Ordering::Acquire));
        assert!(!windows.park(id), "the new session is already waiting");
        assert!(matches!(rx.try_recv(), Ok(Control::Attach(1))));
    }

    #[test]
    fn claims_skip_windows_whose_thread_ended() {
        let windows = Windows::<u32>::default();
        let (tx, rx) = mpsc::channel();
        let id = windows.insert(key(None, [10, 0, 0, 1]), tx);
        windows.park(id);
        drop(rx);
        assert_eq!(windows.claim(&key(None, [10, 0, 0, 1]), 3), Err(3));
        assert_eq!(windows.parked(), 0);
    }

    #[test]
    fn single_device_mode_closes_other_parked_windows() {
        let windows = Windows::<u32>::default();
        let (ta, ra) = mpsc::channel();
        let (tb, rb) = mpsc::channel();
        let a = windows.insert(key(None, [10, 0, 0, 1]), ta);
        let b = windows.insert(key(None, [10, 0, 0, 2]), tb);
        windows.park(a);
        windows.park(b);
        assert_eq!(windows.close_parked_except(&key(None, [10, 0, 0, 2])), 1);
        assert!(matches!(ra.try_recv(), Ok(Control::Close)));
        assert!(rb.try_recv().is_err());
        assert!(windows.retire(a));
        let threads = windows.close_all();
        assert!(threads.is_empty());
        assert!(matches!(rb.try_recv(), Ok(Control::Close)));
    }
}
