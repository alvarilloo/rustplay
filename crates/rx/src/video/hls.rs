// HLS playlists handed over by the client through FCUP requests (the YouTube app). The client's
// master playlist lists media playlists under a private prefix such as "mlhls://localhost"; we
// fetch each one through the reverse channel, then serve all of them to the local player from this
// receiver over loopback, with the prefix replaced by our own address.

pub const MASTER_PATH: &str = "/master.m3u8";

// The playlist as text ending in a newline, or None when it is not an M3U8 playlist.
pub fn normalize(data: &[u8]) -> Option<String> {
    let mut text = String::from_utf8_lossy(data).replace("\r\n", "\n");
    if !text.starts_with("#EXTM3U") {
        return None;
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

// Every distinct "<prefix>...m3u8" URI in a master playlist (on URI lines or in URI="..."
// attributes), in order of appearance.
pub fn media_uris(master: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if prefix.is_empty() {
        return out;
    }
    let mut rest = master;
    while let Some(start) = rest.find(prefix) {
        let tail = &rest[start..];
        let token_end = tail.find(['\n', '\r', '"', ',']).unwrap_or(tail.len());
        let token = &tail[..token_end];
        if let Some(end) = token.find("m3u8") {
            let uri = &token[..end + 4];
            if !out.iter().any(|u| u == uri) {
                out.push(uri.to_owned());
            }
        }
        rest = &tail[token_end.max(prefix.len())..];
    }
    out
}

// The master playlist as the local player must see it.
pub fn rewrite_master(master: &str, prefix: &str, local: &str) -> String {
    master.replace(prefix, local)
}

// Value of attribute `key` in a tag line such as #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="a",...
pub fn attribute<'a>(line: &'a str, key: &str) -> Option<&'a str> {
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
            return Some(value);
        }
        rest = next.strip_prefix(',').unwrap_or(next);
        if rest.is_empty() {
            return None;
        }
    }
}

fn primary(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

// When a group of alternative audio renditions offers several languages (YouTube's dubbed
// tracks), keeps a single one per group: the first preferred language, else the default one, else
// the first listed. Groups are never emptied, so every variant stream keeps its audio.
// True when Windows' player can decode every codec of a variant (H.264 video, AAC audio).
// YouTube also lists VP9 (itag 600+), AV1 and HEVC variants that it cannot read inside HLS.
fn playable_codecs(codecs: &str) -> bool {
    codecs.split(',').map(str::trim).all(|c| {
        let c = c.to_ascii_lowercase();
        c.starts_with("avc1") || c.starts_with("avc3") || c.starts_with("mp4a")
    })
}

// Removes the variants the player cannot decode. A master without any playable variant (or
// without codec information) comes back unchanged, so the player can still try.
pub fn playable_only(master: &str) -> String {
    let mut out = String::with_capacity(master.len());
    let mut kept = 0;
    let mut removed = 0;
    let mut skip_uri = false;
    for line in master.lines() {
        if skip_uri {
            skip_uri = false;
            if !line.starts_with('#') {
                continue;
            }
        }
        let variant = line.starts_with("#EXT-X-STREAM-INF");
        if variant || line.starts_with("#EXT-X-I-FRAME-STREAM-INF") {
            if let Some(codecs) = attribute(line, "CODECS")
                && !playable_codecs(codecs)
            {
                removed += 1;
                skip_uri = variant;
                continue;
            }
            if variant {
                kept += 1;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if kept == 0 || removed == 0 {
        return master.to_owned();
    }
    tracing::info!("se descartan {removed} calidades que Windows no puede reproducir (VP9, AV1 o HEVC); quedan {kept}");
    out
}

pub fn select_audio_language(master: &str, preferred: &[String]) -> String {
    let lines: Vec<&str> = master.lines().collect();
    let audio: Vec<(usize, &str, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("#EXT-X-MEDIA:") && attribute(l, "TYPE") == Some("AUDIO"))
        .map(|(i, l)| (i, attribute(l, "GROUP-ID").unwrap_or(""), attribute(l, "LANGUAGE").unwrap_or("")))
        .collect();
    let mut drop = vec![false; lines.len()];
    let mut groups: Vec<&str> = audio.iter().map(|(_, g, _)| *g).collect();
    groups.dedup();
    groups.sort_unstable();
    groups.dedup();
    for group in groups {
        let members: Vec<&(usize, &str, &str)> = audio.iter().filter(|(_, g, _)| *g == group).collect();
        let first_language = members[0].2;
        if members.iter().all(|(_, _, lang)| primary(lang).eq_ignore_ascii_case(primary(first_language))) {
            continue;
        }
        let by_preference = preferred.iter().find_map(|want| {
            members.iter().find(|(_, _, lang)| !lang.is_empty() && primary(lang).eq_ignore_ascii_case(primary(want)))
        });
        let by_default = || members.iter().find(|(i, _, _)| attribute(lines[*i], "DEFAULT") == Some("YES"));
        let keep = by_preference.or_else(by_default).unwrap_or(&members[0]);
        let keep_language = keep.2;
        for (i, _, lang) in &members {
            if !primary(lang).eq_ignore_ascii_case(primary(keep_language)) {
                drop[*i] = true;
            }
        }
    }
    let mut out = String::with_capacity(master.len());
    for (i, line) in lines.iter().enumerate() {
        if !drop[i] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

// Expands YouTube's "#YT-EXT-CONDENSED-URL" media playlists into the standard form:
//   #YT-EXT-CONDENSED-URL:BASE-URI="https://h/videoplayback",PARAMS="id,itag,sq",PREFIX="s/"
//   s/abc/136/7  ->  https://h/videoplayback/id/abc/itag/136/sq/7
// Playlists without that tag come back unchanged; None when the tag is malformed.
pub fn expand_condensed(playlist: &str) -> Option<String> {
    let first_segment = playlist.find("#EXTINF").unwrap_or(playlist.len());
    let Some(tag_line) = playlist[..first_segment].lines().find(|l| l.starts_with("#YT-EXT-CONDENSED-URL")) else {
        return Some(playlist.to_owned());
    };
    let base = attribute(tag_line, "BASE-URI").filter(|v| !v.is_empty())?;
    let params: Vec<&str> = attribute(tag_line, "PARAMS").filter(|v| !v.is_empty())?.split(',').collect();
    let prefix = attribute(tag_line, "PREFIX").filter(|v| !v.is_empty())?;
    let mut out = String::with_capacity(playlist.len() * 2);
    let mut in_segments = false;
    for line in playlist.lines() {
        if line.starts_with("#EXTINF") {
            in_segments = true;
        }
        match line.strip_prefix(prefix) {
            Some(rest) if in_segments && !line.starts_with('#') => {
                let values: Vec<&str> = rest.splitn(params.len(), '/').collect();
                if values.len() != params.len() {
                    return None;
                }
                out.push_str(base);
                for (param, value) in params.iter().zip(values) {
                    out.push('/');
                    out.push_str(param);
                    out.push('/');
                    out.push_str(value);
                }
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    Some(out)
}

// Number of segments, total duration in seconds and whether the playlist is complete.
pub fn analyze_media(playlist: &str) -> (usize, f64, bool) {
    let mut count = 0;
    let mut duration = 0.0;
    for line in playlist.lines() {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            count += 1;
            duration += rest.split(',').next().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        }
    }
    (count, duration, playlist.contains("#EXT-X-ENDLIST"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-MEDIA:URI=\"mlhls://localhost/itag/234/en.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\",NAME=\"English\",DEFAULT=YES,AUTOSELECT=YES,LANGUAGE=\"en\"\n\
#EXT-X-MEDIA:URI=\"mlhls://localhost/itag/234/es.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\",NAME=\"Español\",DEFAULT=NO,AUTOSELECT=YES,LANGUAGE=\"es-419\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720,AUDIO=\"234\"\n\
mlhls://localhost/itag/136.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"234\"\n\
mlhls://localhost/itag/137.m3u8\n";

    #[test]
    fn keeps_only_variants_windows_can_play() {
        let master = "#EXTM3U\n\
#EXT-X-MEDIA:URI=\"mlhls://localhost/itag/234/a.m3u8\",TYPE=AUDIO,GROUP-ID=\"234\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"avc1.4D401F,mp4a.40.2\",AUDIO=\"234\"\n\
mlhls://localhost/itag/231/m.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2,CODECS=\"vp09.00.21.08,mp4a.40.2\",AUDIO=\"234\"\n\
mlhls://localhost/itag/602/m.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=3,CODECS=\"av01.0.05M.08,mp4a.40.2\",AUDIO=\"234\"\n\
mlhls://localhost/itag/394/m.m3u8\n";
        let kept = playable_only(master);
        assert!(kept.contains("itag/231/"));
        assert!(kept.contains("itag/234/a.m3u8"), "audio renditions stay");
        assert!(!kept.contains("itag/602/") && !kept.contains("vp09"));
        assert!(!kept.contains("itag/394/") && !kept.contains("av01"));
        // Nothing playable: unchanged, so the player can still try.
        let only_vp9 = "#EXTM3U\n#EXT-X-STREAM-INF:CODECS=\"vp09.00.21.08\"\nmlhls://localhost/x.m3u8\n";
        assert_eq!(playable_only(only_vp9), only_vp9);
    }

    #[test]
    fn normalizes_playlists() {
        assert_eq!(normalize(b"#EXTM3U\r\n#EXT-X-VERSION:3").as_deref(), Some("#EXTM3U\n#EXT-X-VERSION:3\n"));
        assert_eq!(normalize(b"<html>"), None);
    }

    #[test]
    fn lists_media_playlists_of_the_master() {
        let uris = media_uris(MASTER, "mlhls://localhost");
        assert_eq!(
            uris,
            [
                "mlhls://localhost/itag/234/en.m3u8",
                "mlhls://localhost/itag/234/es.m3u8",
                "mlhls://localhost/itag/136.m3u8",
                "mlhls://localhost/itag/137.m3u8",
            ]
        );
        let twice = format!("{MASTER}mlhls://localhost/itag/136.m3u8\n");
        assert_eq!(media_uris(&twice, "mlhls://localhost").len(), 4);
        assert!(media_uris(MASTER, "").is_empty());
        assert!(media_uris("#EXTM3U\nmlhls://localhost/nothing\n", "mlhls://localhost").is_empty());
    }

    #[test]
    fn rewrites_the_master_for_the_local_player() {
        let local = rewrite_master(MASTER, "mlhls://localhost", "http://127.0.0.1:7000");
        assert!(!local.contains("mlhls://"));
        assert!(local.contains("URI=\"http://127.0.0.1:7000/itag/234/en.m3u8\""));
        assert!(local.contains("\nhttp://127.0.0.1:7000/itag/137.m3u8\n"));
    }

    #[test]
    fn reads_attributes() {
        let line = "#EXT-X-MEDIA:URI=\"a,b\",TYPE=AUDIO,GROUP-ID=\"g\",DEFAULT=YES";
        assert_eq!(attribute(line, "URI"), Some("a,b"));
        assert_eq!(attribute(line, "TYPE"), Some("AUDIO"));
        assert_eq!(attribute(line, "GROUP-ID"), Some("g"));
        assert_eq!(attribute(line, "DEFAULT"), Some("YES"));
        assert_eq!(attribute(line, "LANGUAGE"), None);
    }

    #[test]
    fn keeps_one_audio_language_per_group() {
        let spanish = select_audio_language(MASTER, &["es-ES".to_owned()]);
        assert!(spanish.contains("LANGUAGE=\"es-419\""));
        assert!(!spanish.contains("LANGUAGE=\"en\""));
        assert!(spanish.contains("mlhls://localhost/itag/137.m3u8\n"));
        let fallback = select_audio_language(MASTER, &["fr".to_owned()]);
        assert!(fallback.contains("LANGUAGE=\"en\""));
        assert!(!fallback.contains("LANGUAGE=\"es-419\""));
        let single = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",LANGUAGE=\"en\",URI=\"x.m3u8\"\nv.m3u8\n";
        assert_eq!(select_audio_language(single, &["es".to_owned()]), single);
    }

    #[test]
    fn expands_youtube_condensed_playlists() {
        let condensed = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:5\n\
#YT-EXT-CONDENSED-URL:BASE-URI=\"https://r1.googlevideo.com/videoplayback\",PARAMS=\"id,itag,sq\",PREFIX=\"s/\"\n\
#EXTINF:5.005,\n\
s/abc/136/0\n\
#EXTINF:4.5,\n\
s/abc/136/1\n\
#EXT-X-ENDLIST\n";
        let expanded = expand_condensed(condensed).unwrap();
        assert!(expanded.contains("\nhttps://r1.googlevideo.com/videoplayback/id/abc/itag/136/sq/0\n"));
        assert!(expanded.contains("\nhttps://r1.googlevideo.com/videoplayback/id/abc/itag/136/sq/1\n"));
        assert!(expanded.ends_with("#EXT-X-ENDLIST\n"));
        let (count, duration, complete) = analyze_media(&expanded);
        assert_eq!((count, complete), (2, true));
        assert!((duration - 9.505).abs() < 1e-9);

        let plain = "#EXTM3U\n#EXTINF:4,\nhttps://a/b.ts\n";
        assert_eq!(expand_condensed(plain).as_deref(), Some(plain));
        let broken = "#EXTM3U\n#YT-EXT-CONDENSED-URL:BASE-URI=\"https://a\",PREFIX=\"s/\"\n#EXTINF:4,\ns/a\n";
        assert_eq!(expand_condensed(broken), None);
        let short = "#EXTM3U\n#YT-EXT-CONDENSED-URL:BASE-URI=\"https://a\",PARAMS=\"x,y\",PREFIX=\"s/\"\n#EXTINF:4,\ns/a\n";
        assert_eq!(expand_condensed(short), None);
    }
}
