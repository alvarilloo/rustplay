// HLS playlists: the master (video variants and audio renditions) and the media playlists
// (segments), with relative addresses resolved against the playlist's own address.

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub width: u32,
    pub height: u32,
    pub codecs: String,
    pub audio: Option<String>,
    pub subtitles: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SubtitleTrack {
    pub group: String,
    pub name: String,
    pub language: String,
    pub uri: String,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Rendition {
    pub group: String,
    pub uri: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Master {
    pub variants: Vec<Variant>,
    pub audio: Vec<Rendition>,
    pub subtitles: Vec<SubtitleTrack>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Segment {
    pub url: String,
    pub duration: f64,
    // The timeline restarts here (#EXT-X-DISCONTINUITY): the time stamps jump.
    pub discontinuity: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Media {
    pub segments: Vec<Segment>,
    pub ended: bool,
    // fMP4 initialization section (#EXT-X-MAP); the segments are then moof/mdat fragments.
    pub map: Option<String>,
    // Sequence number of the first segment (#EXT-X-MEDIA-SEQUENCE); it grows as a live window slides.
    pub first_sequence: u64,
    pub target_duration: f64,
}

impl Media {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }

    pub fn segment(&self, sequence: u64) -> Option<&Segment> {
        let index = usize::try_from(sequence.checked_sub(self.first_sequence)?).ok()?;
        self.segments.get(index)
    }

    // Target duration in seconds, estimated from the segments when the tag is missing.
    pub fn target(&self) -> f64 {
        if self.target_duration > 0.0 {
            return self.target_duration;
        }
        self.segments.iter().map(|s| s.duration).fold(0.0, f64::max).max(2.0)
    }
}

// The media time of every segment of a playlist, by sequence number. VOD playlists fill it once;
// live playlists keep extending it while their window slides (older segments fall off the front).
// Entries already known never change, so snapshots of other variants of the same stream (whose
// sequence numbers correspond) can be merged into the same timeline.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Timeline {
    first_sequence: u64,
    first_start: f64,
    durations: std::collections::VecDeque<f64>,
}

impl Timeline {
    pub fn new(media: &Media, first_start: f64) -> Self {
        Self {
            first_sequence: media.first_sequence,
            first_start,
            durations: media.segments.iter().map(|s| s.duration).collect(),
        }
    }

    #[cfg(test)]
    pub fn first_sequence(&self) -> u64 {
        self.first_sequence
    }

    pub fn next_sequence(&self) -> u64 {
        self.first_sequence + self.durations.len() as u64
    }

    pub fn window_start(&self) -> f64 {
        self.first_start
    }

    pub fn end(&self) -> f64 {
        self.first_start + self.durations.iter().sum::<f64>()
    }

    pub fn start_of(&self, sequence: u64) -> Option<f64> {
        if sequence < self.first_sequence || sequence > self.next_sequence() {
            return None;
        }
        let n = (sequence - self.first_sequence) as usize;
        Some(self.first_start + self.durations.iter().take(n).sum::<f64>())
    }

    // The segment holding `time` (clamped to the first or last one) and its start.
    pub fn locate(&self, time: f64) -> (u64, f64) {
        let mut start = self.first_start;
        for (i, d) in self.durations.iter().enumerate() {
            if time < start + d || i + 1 == self.durations.len() {
                return (self.first_sequence + i as u64, start);
            }
            start += d;
        }
        (self.first_sequence, self.first_start)
    }

    // Merges a fresh snapshot: forgets segments that slid out of the window and appends new ones.
    pub fn update(&mut self, media: &Media) {
        if self.durations.is_empty() {
            *self = Self::new(media, self.first_start);
            return;
        }
        let end = self.next_sequence();
        if media.first_sequence > end || media.first_sequence + (media.segments.len() as u64) < self.first_sequence {
            // A jump (or a restarted stream): continue the time line from where it ended.
            *self = Self::new(media, self.end());
            return;
        }
        while self.first_sequence < media.first_sequence && !self.durations.is_empty() {
            self.first_start += self.durations.pop_front().unwrap_or(0.0);
            self.first_sequence += 1;
        }
        let known = self.next_sequence();
        for (i, s) in media.segments.iter().enumerate() {
            if media.first_sequence + i as u64 >= known {
                self.durations.push_back(s.duration);
            }
        }
    }

}

// `key="value"` or `key=value` inside the attribute list of a tag line.
pub(super) fn attribute(line: &str, key: &str) -> Option<String> {
    let (_, attrs) = line.split_once(':')?;
    let mut rest = attrs;
    loop {
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        let after = &rest[eq + 1..];
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            let close = quoted.find('"')?;
            (&quoted[..close], &quoted[close + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        if name == key {
            return Some(value.to_owned());
        }
        rest = next.trim_start_matches(',');
        if rest.is_empty() {
            return None;
        }
    }
}

pub(super) fn resolve(base: &str, reference: &str) -> String {
    if reference.contains("://") {
        return reference.to_owned();
    }
    let Some(scheme_end) = base.find("://") else { return reference.to_owned() };
    let after = scheme_end + 3;
    let host_end = base[after..].find('/').map_or(base.len(), |i| after + i);
    if let Some(path) = reference.strip_prefix('/') {
        return format!("{}/{}", &base[..host_end], path);
    }
    let query = base.find('?').unwrap_or(base.len());
    let dir_end = base[..query].rfind('/').filter(|i| *i >= host_end).map_or(host_end, |i| i + 1);
    let dir = &base[..dir_end];
    if dir.ends_with('/') { format!("{dir}{reference}") } else { format!("{dir}/{reference}") }
}

pub(super) fn parse_master(text: &str, base: &str) -> Master {
    let mut master = Master::default();
    let mut pending: Option<Variant> = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with("#EXT-X-STREAM-INF") {
            let (width, height) = attribute(line, "RESOLUTION")
                .and_then(|r| r.split_once('x').map(|(w, h)| (w.parse().unwrap_or(0), h.parse().unwrap_or(0))))
                .unwrap_or((0, 0));
            pending = Some(Variant {
                uri: String::new(),
                bandwidth: attribute(line, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                width,
                height,
                codecs: attribute(line, "CODECS").unwrap_or_default(),
                audio: attribute(line, "AUDIO"),
                subtitles: attribute(line, "SUBTITLES"),
            });
        } else if line.starts_with("#EXT-X-MEDIA") {
            let default = attribute(line, "DEFAULT").as_deref() == Some("YES");
            match attribute(line, "TYPE").as_deref() {
                Some("AUDIO") => master.audio.push(Rendition {
                    group: attribute(line, "GROUP-ID").unwrap_or_default(),
                    uri: attribute(line, "URI").map(|u| resolve(base, &u)),
                    default,
                }),
                Some("SUBTITLES") => {
                    if let Some(uri) = attribute(line, "URI") {
                        let language = attribute(line, "LANGUAGE").unwrap_or_default();
                        let name = attribute(line, "NAME").filter(|n| !n.is_empty()).unwrap_or_else(|| language.clone());
                        master.subtitles.push(SubtitleTrack {
                            group: attribute(line, "GROUP-ID").unwrap_or_default(),
                            name,
                            language,
                            uri: resolve(base, &uri),
                            default,
                        });
                    }
                }
                _ => {}
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(mut variant) = pending.take() {
                variant.uri = resolve(base, line);
                master.variants.push(variant);
            }
        }
    }
    master
}

pub(super) fn parse_media(text: &str, base: &str) -> Media {
    let mut media = Media::default();
    let mut duration = None;
    let mut discontinuity = false;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest.split(',').next().and_then(|d| d.trim().parse::<f64>().ok());
        } else if line.starts_with("#EXT-X-MAP") {
            media.map = attribute(line, "URI").map(|u| resolve(base, &u));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media.first_sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            media.target_duration = rest.trim().parse().unwrap_or(0.0);
        } else if line.starts_with("#EXT-X-DISCONTINUITY") && !line.starts_with("#EXT-X-DISCONTINUITY-SEQUENCE") {
            discontinuity = true;
        } else if line.starts_with("#EXT-X-ENDLIST") || line.starts_with("#EXT-X-PLAYLIST-TYPE:VOD") {
            media.ended = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(duration) = duration.take() {
                media.segments.push(Segment { url: resolve(base, line), duration, discontinuity: std::mem::take(&mut discontinuity) });
            }
        }
    }
    media
}

fn is_playable_codec(codec: &str) -> bool {
    let c = codec.trim().to_ascii_lowercase();
    c.starts_with("avc1") || c.starts_with("avc3") || c.starts_with("mp4a")
}

// The variants that can be played up to `max_height` lines, worst to best, one per picture
// height (the best of several with the same height). Never empty when anything is playable:
// if every variant is taller than `max_height`, the smallest one stays.
pub(super) fn ladder(master: &Master, max_height: u32) -> Vec<&Variant> {
    let playable = |v: &&Variant| !v.codecs.is_empty() && v.codecs.split(',').all(is_playable_codec) && v.codecs.to_ascii_lowercase().contains("avc");
    let lc = |v: &Variant| v.codecs.to_ascii_lowercase().contains("mp4a.40.2");
    let mut all: Vec<&Variant> = master.variants.iter().filter(playable).collect();
    let mut chosen: Vec<&Variant> = all.iter().copied().filter(|v| v.height == 0 || v.height <= max_height).collect();
    if chosen.is_empty() {
        all.sort_by_key(|v| v.height);
        chosen.extend(all.first().copied());
    }
    let mut out: Vec<&Variant> = Vec::new();
    for v in chosen {
        match out.iter().position(|o| o.height == v.height) {
            Some(i) if (lc(v), v.bandwidth) > (lc(out[i]), out[i].bandwidth) => out[i] = v,
            Some(_) => {}
            None => out.push(v),
        }
    }
    out.sort_by_key(|v| (v.height, v.bandwidth));
    out
}

// Address of the audio playlist for `variant`; None when the audio is inside the video segments.
pub(super) fn audio_for<'a>(master: &'a Master, variant: &Variant) -> Option<&'a str> {
    let group = variant.audio.as_deref()?;
    let renditions: Vec<&Rendition> = master.audio.iter().filter(|r| r.group == group && r.uri.is_some()).collect();
    renditions.iter().find(|r| r.default).or(renditions.first()).and_then(|r| r.uri.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:URI=\"audio/233.m3u8\",TYPE=AUDIO,GROUP-ID=\"233\",DEFAULT=YES,NAME=\"Default\"\n\
#EXT-X-MEDIA:URI=\"audio/234.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\",DEFAULT=YES,NAME=\"Default\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=341325,CODECS=\"avc1.4D4015,mp4a.40.5\",RESOLUTION=426x240,AUDIO=\"233\"\n\
v/229.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=1374556,CODECS=\"avc1.4D401F,mp4a.40.2\",RESOLUTION=854x480,AUDIO=\"234\"\n\
v/231.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=6000000,CODECS=\"avc1.640033,mp4a.40.2\",RESOLUTION=3840x2160,AUDIO=\"234\"\n\
v/4k.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"vp09.00.21.08,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"234\"\n\
v/602.m3u8\n";

    #[test]
    fn resolves_addresses() {
        assert_eq!(resolve("http://h:1/a/b.m3u8", "c.ts"), "http://h:1/a/c.ts");
        assert_eq!(resolve("http://h:1/a/b.m3u8?x=1/2", "c.ts"), "http://h:1/a/c.ts");
        assert_eq!(resolve("http://h:1/a/b.m3u8", "/z/c.ts"), "http://h:1/z/c.ts");
        assert_eq!(resolve("http://h:1/a/b.m3u8", "https://o/c.ts"), "https://o/c.ts");
        assert_eq!(resolve("http://h:1", "c.ts"), "http://h:1/c.ts");
    }

    #[test]
    fn parses_the_master_and_picks_a_variant() {
        let master = parse_master(MASTER, "http://127.0.0.1:7000/master.m3u8");
        assert_eq!(master.variants.len(), 4);
        assert_eq!(master.audio.len(), 2);
        let chosen = *ladder(&master, 1080).last().unwrap();
        assert_eq!(chosen.uri, "http://127.0.0.1:7000/v/231.m3u8", "4K is too big and VP9 cannot be decoded");
        assert_eq!(audio_for(&master, chosen), Some("http://127.0.0.1:7000/audio/234.m3u8"));
        // Nothing small enough: the smallest decodable one.
        assert_eq!(ladder(&master, 100)[0].height, 240);
    }

    #[test]
    fn builds_a_quality_ladder() {
        let master = parse_master(MASTER, "http://127.0.0.1:7000/master.m3u8");
        let heights: Vec<u32> = ladder(&master, 1080).iter().map(|v| v.height).collect();
        assert_eq!(heights, [240, 480], "4K is too big and VP9 cannot be decoded");
        // With only choices taller than the limit, the smallest stays.
        assert_eq!(ladder(&master, 100).iter().map(|v| v.height).collect::<Vec<_>>(), [240]);
        // Several variants with one height: the AAC-LC one with the higher bitrate wins.
        let text = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"avc1.4D401F,mp4a.40.5\",RESOLUTION=854x480\na.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"avc1.4D401F,mp4a.40.2\",RESOLUTION=854x480\nb.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=1800000,CODECS=\"avc1.4D401F,mp4a.40.2\",RESOLUTION=1280x720\nc.m3u8\n";
        let m = parse_master(text, "http://h/m.m3u8");
        let l = ladder(&m, 1080);
        assert_eq!(l.iter().map(|v| v.uri.as_str()).collect::<Vec<_>>(), ["http://h/b.m3u8", "http://h/c.m3u8"]);
    }

    #[test]
    fn parses_subtitle_renditions() {
        let text = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Español\",LANGUAGE=\"es\",DEFAULT=NO,URI=\"subs/es.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"https://o/en.vtt\"\n\
#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",INSTREAM-ID=\"CC1\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"avc1.4D401F,mp4a.40.2\",RESOLUTION=854x480,SUBTITLES=\"subs\"\na.m3u8\n";
        let master = parse_master(text, "http://h/p/m.m3u8");
        assert_eq!(master.subtitles.len(), 2);
        assert_eq!(master.subtitles[0].name, "Español");
        assert_eq!(master.subtitles[0].uri, "http://h/p/subs/es.m3u8");
        assert_eq!(master.subtitles[1].name, "en", "the language names a track without NAME");
        assert!(master.subtitles[1].default);
        assert_eq!(master.variants[0].subtitles.as_deref(), Some("subs"));
    }

    #[test]
    fn parses_live_and_fmp4_playlists() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:120\n\
#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\ns120.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:3.5,\ns121.m4s\n";
        let media = parse_media(text, "http://h/live/p.m3u8");
        assert!(!media.ended);
        assert_eq!(media.first_sequence, 120);
        assert_eq!(media.target(), 4.0);
        assert_eq!(media.map.as_deref(), Some("http://h/live/init.mp4"));
        assert!(!media.segments[0].discontinuity && media.segments[1].discontinuity);
        assert_eq!(media.segment(121).unwrap().url, "http://h/live/s121.m4s");
        assert!(media.segment(119).is_none() && media.segment(122).is_none());
    }

    #[test]
    fn the_timeline_follows_a_sliding_window() {
        let snapshot = |first: u64, count: u64| Media {
            first_sequence: first,
            segments: (0..count).map(|i| Segment { url: format!("s{}", first + i), duration: 4.0, discontinuity: false }).collect(),
            ..Default::default()
        };
        let mut t = Timeline::new(&snapshot(10, 3), 0.0);
        assert_eq!((t.window_start(), t.end(), t.next_sequence()), (0.0, 12.0, 13));
        assert_eq!(t.locate(5.0), (11, 4.0));
        assert_eq!(t.locate(100.0), (12, 8.0));
        // The window slides by two segments and gains two: the time line keeps going.
        t.update(&snapshot(12, 3));
        assert_eq!((t.first_sequence(), t.window_start(), t.end()), (12, 8.0, 20.0));
        assert_eq!(t.start_of(14), Some(16.0));
        assert_eq!(t.start_of(11), None);
        // The same snapshot again changes nothing.
        let before = t.clone();
        t.update(&snapshot(12, 3));
        assert_eq!(t, before);
        // A jump far ahead continues from the end.
        t.update(&snapshot(500, 2));
        assert_eq!((t.first_sequence(), t.window_start(), t.end()), (500, 20.0, 28.0));
    }

    #[test]
    fn parses_a_media_playlist() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:5.9,\nseg0.ts\n#EXTINF:6.0,\nhttps://o/seg1.ts?a=b\n#EXTINF:3.0,\nseg2.ts\n#EXT-X-ENDLIST\n";
        let media = parse_media(text, "http://h/p/list.m3u8");
        assert_eq!(media.segments.len(), 3);
        assert_eq!(media.segments[0].url, "http://h/p/seg0.ts");
        assert_eq!(media.segments[1].url, "https://o/seg1.ts?a=b");
        assert!(media.ended);
        assert!((media.duration() - 14.9).abs() < 1e-9);
    }
}
