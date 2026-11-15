// WebVTT subtitles: the parser, the list of cues of the selected track, and the thread that
// downloads them (a single .vtt file or a playlist of .vtt segments, live ones included).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::http;
use super::playlist::{self, SubtitleTrack};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Parsed {
    pub cues: Vec<Cue>,
    // X-TIMESTAMP-MAP: (MPEGTS in 90 kHz ticks, LOCAL in seconds). A cue at local time `t` shows
    // at transport stream time MPEGTS / 90000 + (t - LOCAL).
    pub map: Option<(u64, f64)>,
}

// "[hh:]mm:ss.mmm" in seconds.
fn timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    let (clock, millis) = s.split_once(['.', ','])?;
    let mut parts: Vec<&str> = clock.split(':').collect();
    if parts.len() == 2 {
        parts.insert(0, "0");
    }
    if parts.len() != 3 || millis.is_empty() || !millis.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m, sec): (f64, f64, f64) = (parts[0].parse().ok()?, parts[1].parse().ok()?, parts[2].parse().ok()?);
    let fraction: f64 = format!("0.{millis}").parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec + fraction)
}

// Drops the markup of a cue payload (<i>, <c.yellow>, <00:01.000>, <v Name>) and decodes the
// entities WebVTT allows.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ").replace("&lrm;", "").replace("&rlm;", "")
}

pub(super) fn parse(text: &str) -> Parsed {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let mut parsed = Parsed::default();
    if !text.starts_with("WEBVTT") {
        return parsed;
    }
    for block in text.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        for line in &lines {
            if let Some(rest) = line.strip_prefix("X-TIMESTAMP-MAP=") {
                let mpegts = rest.split(',').find_map(|p| p.trim().strip_prefix("MPEGTS:")).and_then(|v| v.trim().parse().ok());
                let local = rest.split(',').find_map(|p| p.trim().strip_prefix("LOCAL:")).and_then(timestamp);
                if let (Some(mpegts), Some(local)) = (mpegts, local) {
                    parsed.map = Some((mpegts, local));
                }
            }
        }
        // The timing line is the first or second line of a cue block.
        let Some(at) = lines.iter().take(2).position(|l| l.contains("-->")) else { continue };
        if lines[0].starts_with("NOTE") || lines[0].starts_with("STYLE") || lines[0].starts_with("REGION") {
            continue;
        }
        let Some((from, rest)) = lines[at].split_once("-->") else { continue };
        let to = rest.split_whitespace().next().unwrap_or_default();
        let (Some(start), Some(end)) = (timestamp(from), timestamp(to)) else { continue };
        let payload = clean(&lines[at + 1..].join("\n")).trim().to_owned();
        if !payload.is_empty() && end > start {
            parsed.cues.push(Cue { start, end, text: payload });
        }
    }
    parsed
}

// The cues of the selected track, sorted by start time; shared with the loader thread.
#[derive(Default)]
pub(super) struct Cues {
    list: Mutex<Vec<Cue>>,
}

impl Cues {
    pub fn add(&self, cues: impl IntoIterator<Item = Cue>) {
        let Ok(mut list) = self.list.lock() else { return };
        for cue in cues {
            if !list.contains(&cue) {
                let at = list.partition_point(|c| c.start <= cue.start);
                list.insert(at, cue);
            }
        }
    }

    // The text to show at `time`: every active cue, one per line.
    pub fn at(&self, time: f64) -> Option<String> {
        let list = self.list.lock().ok()?;
        let upto = list.partition_point(|c| c.start <= time);
        let active: Vec<&str> = list[..upto].iter().rev().take(8).filter(|c| c.end > time).map(|c| c.text.as_str()).collect();
        (!active.is_empty()).then(|| active.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    pub fn len(&self) -> usize {
        self.list.lock().map_or(0, |l| l.len())
    }
}

// Where the transport stream time stamps of the video are anchored on the media time line:
// (time stamp in 100 ns, media time in microseconds). Set by the video feeder.
pub(super) type Origin = Arc<Mutex<Option<(i64, i64)>>>;

// Shifts cue times of a file with an X-TIMESTAMP-MAP onto the media time line.
pub(super) fn shift_for(map: Option<(u64, f64)>, origin: Option<(i64, i64)>) -> f64 {
    match (map, origin) {
        (Some((mpegts, local)), Some((pts_100ns, us))) => mpegts as f64 / 90_000.0 - local - pts_100ns as f64 / 1e7 + us as f64 / 1e6,
        _ => 0.0,
    }
}

fn is_vtt_address(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".vtt") || path.ends_with(".webvtt")
}

fn sleep(stop: &AtomicBool, duration: Duration) -> bool {
    let until = std::time::Instant::now() + duration;
    while std::time::Instant::now() < until {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !stop.load(Ordering::Acquire)
}

fn fetch_cues(url: &str, origin: &Origin, stop: &AtomicBool) -> Option<Vec<Cue>> {
    let bytes = match http::get(url) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!("no se pudo descargar un fragmento de subtítulos: {e}");
            return None;
        }
    };
    let parsed = parse(&String::from_utf8_lossy(&bytes));
    let mut shift = 0.0;
    if parsed.map.is_some() {
        // The video's time stamps are known once its first segment has been read.
        for _ in 0..100 {
            let known = origin.lock().ok().and_then(|o| *o);
            if known.is_some() || stop.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        shift = shift_for(parsed.map, origin.lock().ok().and_then(|o| *o));
    }
    Some(parsed.cues.into_iter().map(|c| Cue { start: c.start + shift, end: c.end + shift, text: c.text }).collect())
}

// Downloads the track into `cues` until it is complete or `stop` is set.
pub(super) fn load(track: SubtitleTrack, cues: Arc<Cues>, origin: Origin, stop: Arc<AtomicBool>) {
    tracing::info!("subtítulos: descargando «{}» ({})", track.name, track.language);
    if is_vtt_address(&track.uri) {
        if let Some(list) = fetch_cues(&track.uri, &origin, &stop) {
            cues.add(list);
        }
        return;
    }
    let mut next: Option<u64> = None;
    let mut failures = 0;
    while !stop.load(Ordering::Acquire) {
        let media = match http::get(&track.uri) {
            Ok(b) => playlist::parse_media(&String::from_utf8_lossy(&b), &track.uri),
            Err(e) => {
                failures += 1;
                tracing::debug!("no se pudo descargar la lista de subtítulos: {e}");
                if failures >= 5 || !sleep(&stop, Duration::from_secs(2)) {
                    return;
                }
                continue;
            }
        };
        for (i, segment) in media.segments.iter().enumerate() {
            let sequence = media.first_sequence + i as u64;
            if next.is_some_and(|n| sequence < n) {
                continue;
            }
            if stop.load(Ordering::Acquire) {
                return;
            }
            if let Some(list) = fetch_cues(&segment.url, &origin, &stop) {
                cues.add(list);
            }
            next = Some(sequence + 1);
        }
        if media.ended {
            tracing::info!("subtítulos: {} líneas cargadas", cues.len());
            return;
        }
        if !sleep(&stop, Duration::from_secs_f64(media.target().clamp(1.0, 10.0))) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\nNOTE a comment\n\n1\n00:00:01.000 --> 00:00:03.500 align:start position:0%\nHola, <i>mundo</i>\n&amp; adiós\n\n00:03.600 --> 00:05.000\n<v Ana>Segunda <c.yellow>línea</c>\n\n00:00:09.000 --> 00:00:08.000\nbackwards\n\n01:02:03.250 --> 01:02:04.000\nlarga\n";

    #[test]
    fn parses_cues_timestamps_and_markup() {
        let p = parse(SAMPLE);
        assert_eq!(p.map, Some((900_000, 0.0)));
        assert_eq!(p.cues.len(), 3, "the backwards cue is dropped: {:?}", p.cues);
        assert_eq!(p.cues[0], Cue { start: 1.0, end: 3.5, text: "Hola, mundo\n& adiós".into() });
        assert_eq!(p.cues[1], Cue { start: 3.6, end: 5.0, text: "Segunda línea".into() });
        assert!((p.cues[2].start - 3723.25).abs() < 1e-9);
        assert!(parse("not a subtitle file").cues.is_empty());
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp("00:00:01.500"), Some(1.5));
        assert_eq!(timestamp("02:03.040"), Some(123.04));
        assert_eq!(timestamp("00:00:01,5"), Some(1.5));
        assert_eq!(timestamp("1.5"), None);
        assert_eq!(timestamp("aa:bb:cc.ddd"), None);
    }

    #[test]
    fn the_current_cue_is_found_even_with_overlaps() {
        let cues = Cues::default();
        cues.add(parse(SAMPLE).cues);
        assert_eq!(cues.at(0.5), None);
        assert_eq!(cues.at(2.0).as_deref(), Some("Hola, mundo\n& adiós"));
        assert_eq!(cues.at(3.55), None);
        assert_eq!(cues.at(4.0).as_deref(), Some("Segunda línea"));
        // Overlapping cues show together, oldest first; duplicates from segment overlaps are ignored.
        cues.add([Cue { start: 3.0, end: 4.5, text: "solapada".into() }, Cue { start: 3.0, end: 4.5, text: "solapada".into() }]);
        assert_eq!(cues.at(4.0).as_deref(), Some("solapada\nSegunda línea"));
        assert_eq!(cues.len(), 4);
    }

    #[test]
    fn timestamp_map_shifts_cues_onto_the_media_time_line() {
        // MPEGTS 10 s with LOCAL 0 and a video whose first picture (PTS 10 s) is media time 0.
        assert!((shift_for(Some((900_000, 0.0)), Some((100_000_000, 0))) - 0.0).abs() < 1e-9);
        // The video starts at PTS 12 s: cues are 2 s earlier on the line.
        assert!((shift_for(Some((900_000, 0.0)), Some((120_000_000, 0))) + 2.0).abs() < 1e-9);
        // The video was anchored at media time 30 s (a seek).
        assert!((shift_for(Some((900_000, 0.0)), Some((100_000_000, 30_000_000))) - 30.0).abs() < 1e-9);
        assert_eq!(shift_for(None, Some((1, 1))), 0.0);
        assert_eq!(shift_for(Some((1, 0.0)), None), 0.0);
    }

    #[test]
    fn recognises_vtt_addresses() {
        assert!(is_vtt_address("https://x/a/b.vtt?lang=es"));
        assert!(!is_vtt_address("https://x/a/b.m3u8"));
    }
}
