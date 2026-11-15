use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_LOG: u32 = WM_APP + 4;
const CAPACITY: usize = 3000;

#[derive(Clone)]
pub struct LogEntry {
    pub id: u64,
    pub time: String,
    pub level: Level,
    pub text: String,
}

pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    next_id: AtomicU64,
    hwnd: AtomicIsize,
    pending: AtomicBool,
    // 0 = debug and above, 1 = info and above.
    verbosity: AtomicU8,
}

static BUFFER: OnceLock<LogBuffer> = OnceLock::new();

pub fn buffer() -> &'static LogBuffer {
    BUFFER.get_or_init(|| LogBuffer {
        entries: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        next_id: AtomicU64::new(0),
        hwnd: AtomicIsize::new(0),
        pending: AtomicBool::new(false),
        verbosity: AtomicU8::new(1),
    })
}

impl LogBuffer {
    pub fn attach(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as isize, Ordering::Release);
    }

    pub fn set_verbose(&self, verbose: bool) {
        self.verbosity.store(u8::from(!verbose), Ordering::Release);
    }

    pub fn verbose(&self) -> bool {
        self.verbosity.load(Ordering::Acquire) == 0
    }

    fn accepts(&self, level: &Level) -> bool {
        *level <= Level::INFO || (self.verbose() && *level <= Level::DEBUG)
    }

    // Called by the UI after handling WM_LOG so the next event posts again.
    pub fn acknowledge(&self) {
        self.pending.store(false, Ordering::Release);
    }

    pub fn snapshot(&self, min: Level) -> Vec<LogEntry> {
        let Ok(entries) = self.entries.lock() else { return Vec::new() };
        entries.iter().filter(|e| e.level <= min).cloned().collect()
    }

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    fn push(&self, level: Level, text: String) {
        let t = unsafe { GetLocalTime() };
        let entry = LogEntry {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            time: format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds),
            level,
            text,
        };
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() == CAPACITY {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 && !self.pending.swap(true, Ordering::AcqRel) {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), WM_LOG, WPARAM(0), LPARAM(0));
            }
        }
    }
}

#[derive(Default)]
struct Message {
    text: String,
    fields: String,
}

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.text.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.text, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

// Keeps the latest log lines in memory for the «Registro» page.
pub struct LiveLayer;

impl<S: Subscriber> Layer<S> for LiveLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let buffer = buffer();
        let level = *event.metadata().level();
        if !buffer.accepts(&level) {
            return;
        }
        let mut message = Message::default();
        event.record(&mut message);
        message.text.push_str(&message.fields);
        buffer.push(level, message.text);
    }
}
