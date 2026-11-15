// "Now playing" details that an iPhone sends with SET_PARAMETER while it streams audio
// (not while mirroring): DMAP-tagged track info, cover art and RTP-time progress.
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    // Encoded image as sent (JPEG or PNG); `None` when the track has no artwork.
    pub artwork: Option<Arc<[u8]>>,
    pub position_ms: Option<u32>,
    pub duration_ms: Option<u32>,
    // False after a FLUSH (pause) until the device sends audio progress again.
    pub playing: bool,
    // The device advertised DACP, so `ReceiverHandle::remote` can control it.
    pub remote: bool,
}

impl NowPlaying {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none() && self.artwork.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

// DMAP containers that may wrap the track tags.
const CONTAINERS: [&[u8; 4]; 3] = [b"mlit", b"mlcl", b"cmst"];

// Walks a DMAP buffer (4-byte tag, 4-byte big-endian length, payload) and keeps the
// title (minm), artist (asar) and album (asal). Malformed tails are ignored.
pub fn parse_dmap(body: &[u8]) -> TrackInfo {
    let mut info = TrackInfo::default();
    walk(body, &mut info, 0);
    info
}

fn walk(mut data: &[u8], info: &mut TrackInfo, depth: u8) {
    while data.len() >= 8 {
        let tag: &[u8; 4] = data[..4].try_into().unwrap_or(&[0; 4]);
        let len = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let Some(payload) = data.get(8..8 + len) else { return };
        if CONTAINERS.contains(&tag) && depth < 4 {
            walk(payload, info, depth + 1);
        } else {
            let slot = match tag {
                b"minm" => Some(&mut info.title),
                b"asar" => Some(&mut info.artist),
                b"asal" => Some(&mut info.album),
                _ => None,
            };
            if let Some(slot) = slot {
                let text = String::from_utf8_lossy(payload).trim().to_owned();
                *slot = (!text.is_empty()).then_some(text);
            }
        }
        data = &data[8 + len..];
    }
}

// "progress: start/current/end" are RTP timestamps; returns (position, duration) in ms.
pub fn progress_ms(start: u32, current: u32, end: u32, sample_rate: u32) -> (u32, u32) {
    let rate = u64::from(sample_rate.max(1));
    let to_ms = |samples: u32| (u64::from(samples) * 1000 / rate).min(u64::from(u32::MAX)) as u32;
    let duration = to_ms(end.wrapping_sub(start));
    let position = to_ms(current.wrapping_sub(start)).min(duration);
    (position, duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = tag.to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn dmap_reads_title_artist_and_album_inside_mlit() {
        let mut inner = item(b"asai", &[0, 0, 0, 0, 0, 0, 0, 1]);
        inner.extend(item(b"minm", "Canción de prueba".as_bytes()));
        inner.extend(item(b"asar", b"Artista"));
        inner.extend(item(b"asal", "Álbum".as_bytes()));
        inner.extend(item(b"caps", &[1]));
        let body = item(b"mlit", &inner);
        let info = parse_dmap(&body);
        assert_eq!(info.title.as_deref(), Some("Canción de prueba"));
        assert_eq!(info.artist.as_deref(), Some("Artista"));
        assert_eq!(info.album.as_deref(), Some("Álbum"));
    }

    #[test]
    fn dmap_accepts_top_level_tags_and_skips_empty_values() {
        let mut body = item(b"minm", b"Title");
        body.extend(item(b"asar", b""));
        let info = parse_dmap(&body);
        assert_eq!(info.title.as_deref(), Some("Title"));
        assert_eq!(info.artist, None);
    }

    #[test]
    fn dmap_survives_truncated_and_garbage_input() {
        let mut body = item(b"minm", b"Ok");
        body.extend_from_slice(b"asar\x00\x00\x10\x00short");
        assert_eq!(parse_dmap(&body).title.as_deref(), Some("Ok"));
        assert_eq!(parse_dmap(&[]), TrackInfo::default());
        assert_eq!(parse_dmap(&[1, 2, 3]), TrackInfo::default());
    }

    #[test]
    fn progress_converts_rtp_times() {
        assert_eq!(progress_ms(44_100, 44_100 * 31, 44_100 * 181, 44_100), (30_000, 180_000));
    }

    #[test]
    fn progress_handles_rtp_wraparound_and_overshoot() {
        let start = u32::MAX - 44_100 + 1;
        assert_eq!(progress_ms(start, 44_100, 44_100 * 9, 44_100), (2_000, 10_000));
        assert_eq!(progress_ms(0, 44_100 * 20, 44_100 * 10, 44_100), (10_000, 10_000));
    }
}
