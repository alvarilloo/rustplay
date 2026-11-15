// Persistent session history: a capped JSON file in the data folder, written atomically.
// Addresses are stored as they are (like the log); only the GUI view honours "hide IPs".

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub const MAX_ENTRIES: usize = 500;
const FILE: &str = "history.json";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    // A device connected to the receiver.
    Connected,
    MirrorStarted,
    MirrorStopped,
    AudioStarted,
    AudioStopped,
    VideoStarted,
    VideoStopped,
    // A finished sending session to a TV.
    Sent,
    Recording,
    Screenshot,
    // Something stopped because of an error.
    Error,
}

const KINDS: [(Kind, &str); 11] = [
    (Kind::Connected, "connected"),
    (Kind::MirrorStarted, "mirror_started"),
    (Kind::MirrorStopped, "mirror_stopped"),
    (Kind::AudioStarted, "audio_started"),
    (Kind::AudioStopped, "audio_stopped"),
    (Kind::VideoStarted, "video_started"),
    (Kind::VideoStopped, "video_stopped"),
    (Kind::Sent, "sent"),
    (Kind::Recording, "recording"),
    (Kind::Screenshot, "screenshot"),
    (Kind::Error, "error"),
];

impl Kind {
    fn tag(self) -> &'static str {
        KINDS.iter().find(|(k, _)| *k == self).map_or("", |(_, tag)| tag)
    }

    fn from_tag(tag: &str) -> Option<Self> {
        KINDS.iter().find(|(_, t)| *t == tag).map(|(k, _)| *k)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    // Seconds since the Unix epoch (UTC).
    pub ts: u64,
    pub kind: Kind,
    // Device name (the TV for `Sent`, the sender for receiver events).
    pub name: String,
    // Network address as the log shows it.
    pub address: String,
    // File of a recording or screenshot.
    pub path: String,
    pub size: u64,
    pub secs: u64,
    // `Sent`: only a window was captured, not the whole screen.
    pub window: bool,
    // `Error`: what went wrong.
    pub message: String,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Entry {
    pub fn new(kind: Kind) -> Self {
        Self::at(kind, now())
    }

    pub fn at(kind: Kind, ts: u64) -> Self {
        Self { ts, kind, name: String::new(), address: String::new(), path: String::new(), size: 0, secs: 0, window: false, message: String::new() }
    }

    pub fn named(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    pub fn address(mut self, address: &str) -> Self {
        self.address = address.to_owned();
        self
    }

    pub fn file(mut self, path: &Path) -> Self {
        self.path = path.display().to_string();
        self.size = std::fs::metadata(path).map_or(0, |m| m.len());
        self
    }

    pub fn secs(mut self, secs: u64) -> Self {
        self.secs = secs;
        self
    }

    pub fn window(mut self, window: bool) -> Self {
        self.window = window;
        self
    }

    pub fn message(mut self, message: &str) -> Self {
        self.message = message.to_owned();
        self
    }

    fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("t".into(), self.ts.into());
        o.insert("k".into(), self.kind.tag().into());
        for (key, text) in [("n", &self.name), ("a", &self.address), ("p", &self.path), ("m", &self.message)] {
            if !text.is_empty() {
                o.insert(key.into(), text.as_str().into());
            }
        }
        if self.size > 0 {
            o.insert("s".into(), self.size.into());
        }
        if self.secs > 0 {
            o.insert("d".into(), self.secs.into());
        }
        if self.window {
            o.insert("w".into(), true.into());
        }
        Value::Object(o)
    }

    fn from_json(value: &Value) -> Option<Self> {
        let o = value.as_object()?;
        let text = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let number = |k: &str| o.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            ts: o.get("t").and_then(Value::as_u64)?,
            kind: Kind::from_tag(o.get("k").and_then(Value::as_str)?)?,
            name: text("n"),
            address: text("a"),
            path: text("p"),
            size: number("s"),
            secs: number("d"),
            window: o.get("w").and_then(Value::as_bool).unwrap_or(false),
            message: text("m"),
        })
    }
}

// Entries oldest first, as stored.
pub struct Store {
    entries: Vec<Entry>,
    path: Option<PathBuf>,
}

impl Store {
    // The history in the data folder (an empty one when the folder is unavailable).
    pub fn open() -> Self {
        match rustplay_core::identity::data_dir() {
            Ok(dir) => Self::load_from(dir.join(FILE)),
            Err(_) => Self::in_memory(),
        }
    }

    // Never touches the disk.
    pub fn in_memory() -> Self {
        Self { entries: Vec::new(), path: None }
    }

    // A missing, unreadable or corrupt file gives an empty history; individual bad entries are skipped.
    pub fn load_from(path: PathBuf) -> Self {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v.get("entries").and_then(Value::as_array).map(|list| list.iter().filter_map(Entry::from_json).collect::<Vec<_>>()))
            .unwrap_or_default();
        let mut store = Self { entries, path: Some(path) };
        store.trim();
        store
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn append(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.trim();
        self.save();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    fn trim(&mut self) {
        if self.entries.len() > MAX_ENTRIES {
            let extra = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..extra);
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = self.write(path) {
            tracing::warn!("no se pudo guardar el historial: {e}");
        }
    }

    // Writes a temporary file next to the real one and renames it over it.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut root = Map::new();
        root.insert("version".into(), 1.into());
        root.insert("entries".into(), Value::Array(self.entries.iter().map(Entry::to_json).collect()));
        let bytes = serde_json::to_vec(&Value::Object(root)).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

// Entries of one local day, newest first. `index` is the position in the slice that was grouped.
pub struct DayGroup<'a> {
    // Days since 1970-01-01 in local time.
    pub day: i64,
    pub items: Vec<(usize, &'a Entry)>,
}

// Groups by local day (`offset_secs` is local time minus UTC), days and entries newest first.
pub fn group_by_day(entries: &[Entry], offset_secs: i64) -> Vec<DayGroup<'_>> {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse((entries[i].ts, i)));
    let mut groups: Vec<DayGroup> = Vec::new();
    for i in order {
        let day = (entries[i].ts as i64 + offset_secs).div_euclid(86_400);
        match groups.last_mut() {
            Some(group) if group.day == day => group.items.push((i, &entries[i])),
            _ => groups.push(DayGroup { day, items: vec![(i, &entries[i])] }),
        }
    }
    groups
}

// Hour and minute of a timestamp in local time.
pub fn clock(ts: u64, offset_secs: i64) -> (u32, u32) {
    let secs = (ts as i64 + offset_secs).rem_euclid(86_400) as u32;
    (secs / 3600, secs / 60 % 60)
}

// (year, month, day) of a day number counted from 1970-01-01.
pub fn civil(day: i64) -> (i64, u32, u32) {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

// Local time minus UTC, in seconds, for now.
pub fn local_offset_secs() -> i64 {
    use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut info = TIME_ZONE_INFORMATION::default();
    // SAFETY: the call fills the structure we pass.
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    let bias = i64::from(info.Bias) + if id == TIME_ZONE_ID_DAYLIGHT { i64::from(info.DaylightBias) } else { i64::from(info.StandardBias) };
    -bias * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustplay-history-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE)
    }

    #[test]
    fn append_and_round_trip() {
        let path = temp_file("round");
        let mut store = Store::load_from(path.clone());
        assert!(store.entries().is_empty());
        let full = Entry::at(Kind::Recording, 1_700_000_000).named("iPhone").address("192.168.1.5").secs(61).window(true).message("x");
        let full = Entry { path: "C:\\v\\a.mp4".into(), size: 1234, ..full };
        store.append(full.clone());
        store.append(Entry::at(Kind::Error, 1_700_000_100).message("fallo"));
        let loaded = Store::load_from(path.clone());
        assert_eq!(loaded.entries(), store.entries());
        assert_eq!(loaded.entries()[0], full);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn capped_to_the_last_entries() {
        let path = temp_file("cap");
        let mut store = Store::load_from(path.clone());
        for i in 0..(MAX_ENTRIES as u64 + 25) {
            store.append(Entry::at(Kind::Connected, 1000 + i));
        }
        assert_eq!(store.entries().len(), MAX_ENTRIES);
        assert_eq!(store.entries()[0].ts, 1025);
        assert_eq!(Store::load_from(path.clone()).entries().len(), MAX_ENTRIES);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_is_tolerated() {
        let path = temp_file("corrupt");
        std::fs::write(&path, b"{ not json at all \xff\xfe").unwrap();
        let mut store = Store::load_from(path.clone());
        assert!(store.entries().is_empty());
        store.append(Entry::at(Kind::Sent, 5));
        assert_eq!(Store::load_from(path.clone()).entries().len(), 1);
        // Bad entries are skipped, good ones kept.
        std::fs::write(&path, br#"{"entries":[{"t":1,"k":"sent"},{"t":2,"k":"nope"},7,{"k":"sent"}]}"#).unwrap();
        assert_eq!(Store::load_from(path.clone()).entries().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn write_is_atomic_and_leaves_no_temp_file() {
        let path = temp_file("atomic");
        let mut store = Store::load_from(path.clone());
        store.append(Entry::at(Kind::Sent, 1));
        store.append(Entry::at(Kind::Sent, 2));
        assert!(!path.with_extension("json.tmp").exists());
        assert!(serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).is_ok());
        store.clear();
        assert!(Store::load_from(path.clone()).entries().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn groups_by_local_day_newest_first() {
        let day = 86_400u64;
        let base = 20_000 * day;
        let entries = vec![
            Entry::at(Kind::Sent, base + 100),
            Entry::at(Kind::Sent, base + day + 5),
            Entry::at(Kind::Sent, base + 50),
            Entry::at(Kind::Sent, base + day + 9),
        ];
        let groups = group_by_day(&entries, 0);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].day, 20_001);
        assert_eq!(groups[0].items.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![3, 1]);
        assert_eq!(groups[1].items.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 2]);
        // A +2 h offset moves the late entry of day 0 into the same local day as the next one.
        let late = vec![Entry::at(Kind::Sent, base + day - 60), Entry::at(Kind::Sent, base + day + 10)];
        assert_eq!(group_by_day(&late, 2 * 3600).len(), 1);
        assert_eq!(group_by_day(&late, 0).len(), 2);
    }

    #[test]
    fn civil_dates_and_clock() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(civil(20_407), (2025, 11, 15));
        assert_eq!(clock(3600 * 13 + 60 * 7, 0), (13, 7));
        assert_eq!(clock(3600 * 23, 2 * 3600), (1, 0));
        assert_eq!(clock(60, -3600), (23, 1));
    }
}
