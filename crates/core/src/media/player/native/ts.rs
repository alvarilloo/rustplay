// MPEG transport stream demuxer for HLS segments: finds the H.264 video and AAC (ADTS) audio
// streams through PAT/PMT and returns each PES payload with its presentation time.

const PACKET: usize = 188;
const SYNC: u8 = 0x47;
const STREAM_H264: u8 = 0x1B;
const STREAM_AAC_ADTS: u8 = 0x0F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Packet {
    pub kind: Kind,
    // Presentation time in 100 ns units (the unit of Media Foundation sample times).
    pub pts: Option<i64>,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub(super) struct Demuxer {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video: Vec<u8>,
    audio: Vec<u8>,
    // Other stream types seen in the PMT, for diagnostics.
    pub unsupported: Vec<u8>,
    // "Packed audio" segments (raw ADTS, as YouTube's audio renditions) were seen.
    packed: bool,
    packed_next: Option<i64>,
}

fn pts_100ns(b: &[u8]) -> i64 {
    let ticks = (i64::from(b[0] >> 1) & 7) << 30
        | i64::from(b[1]) << 22
        | (i64::from(b[2]) >> 1) << 15
        | i64::from(b[3]) << 7
        | i64::from(b[4]) >> 1;
    ticks * 1000 / 9
}

// (pts, payload) of a PES packet.
fn parse_pes(pes: &[u8]) -> Option<(Option<i64>, &[u8])> {
    if pes.len() < 9 || pes[..3] != [0, 0, 1] {
        return None;
    }
    let flags = pes[7];
    let header = usize::from(pes[8]);
    let start = 9 + header;
    if pes.len() < start {
        return None;
    }
    let pts = (flags & 0x80 != 0 && header >= 5).then(|| pts_100ns(&pes[9..14]));
    Some((pts, &pes[start..]))
}

impl Demuxer {
    pub fn has_audio(&self) -> bool {
        self.audio_pid.is_some() || self.packed
    }

    // Processes a whole segment and returns its packets in stream order.
    pub fn push_segment(&mut self, data: &[u8]) -> Vec<Packet> {
        if is_packed_audio(data) {
            return self.packed_audio(data);
        }
        let mut out = Vec::new();
        // Resynchronise when the segment does not start on a packet boundary.
        let mut pos = data.iter().position(|&b| b == SYNC).unwrap_or(data.len());
        while pos + PACKET <= data.len() {
            if data[pos] != SYNC {
                match data[pos..].iter().position(|&b| b == SYNC) {
                    Some(skip) => pos += skip,
                    None => break,
                }
                continue;
            }
            self.packet(&data[pos..pos + PACKET], &mut out);
            pos += PACKET;
        }
        self.finish(&mut out);
        out
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<Packet>) {
        let start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let adaptation = (p[3] >> 4) & 3;
        let mut offset = 4;
        if adaptation & 2 != 0 {
            offset += 1 + usize::from(p[4]);
        }
        if adaptation & 1 == 0 || offset >= PACKET {
            return;
        }
        let payload = &p[offset..];
        if pid == 0 {
            if start {
                self.pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if start {
                self.pmt(payload);
            }
        } else if Some(pid) == self.video_pid {
            Self::collect(&mut self.video, Kind::Video, start, payload, out);
        } else if Some(pid) == self.audio_pid {
            Self::collect(&mut self.audio, Kind::Audio, start, payload, out);
        }
    }

    fn collect(buffer: &mut Vec<u8>, kind: Kind, start: bool, payload: &[u8], out: &mut Vec<Packet>) {
        if start {
            Self::emit(buffer, kind, out);
        }
        // Data before the first PES start (a segment cut mid-packet) is useless.
        if start || !buffer.is_empty() {
            buffer.extend_from_slice(payload);
        }
    }

    fn emit(buffer: &mut Vec<u8>, kind: Kind, out: &mut Vec<Packet>) {
        if let Some((pts, data)) = parse_pes(buffer)
            && !data.is_empty()
        {
            out.push(Packet { kind, pts, data: data.to_vec() });
        }
        buffer.clear();
    }

    // A segment that is just ADTS frames, optionally behind an ID3 tag with the start time.
    fn packed_audio(&mut self, data: &[u8]) -> Vec<Packet> {
        let (tagged, body) = parse_id3(data);
        let frames = split_adts(body);
        if frames.is_empty() {
            return Vec::new();
        }
        self.packed = true;
        let duration: i64 = frames.iter().map(|f| 1024 * 10_000_000 / i64::from(f.sample_rate)).sum();
        let pts = tagged.or(self.packed_next).unwrap_or(0);
        self.packed_next = Some(pts + duration);
        vec![Packet { kind: Kind::Audio, pts: Some(pts), data: body.to_vec() }]
    }

    fn finish(&mut self, out: &mut Vec<Packet>) {
        Self::emit(&mut self.video, Kind::Video, out);
        Self::emit(&mut self.audio, Kind::Audio, out);
    }

    fn section(payload: &[u8], table: u8) -> Option<&[u8]> {
        let pointer = usize::from(*payload.first()?);
        let s = payload.get(1 + pointer..)?;
        if s.len() < 8 || s[0] != table {
            return None;
        }
        let length = (usize::from(s[1] & 0x0F) << 8) | usize::from(s[2]);
        s.get(..(3 + length).min(s.len()))
    }

    fn pat(&mut self, payload: &[u8]) {
        let Some(s) = Self::section(payload, 0) else { return };
        let end = s.len().saturating_sub(4);
        let mut i = 8;
        while i + 4 <= end {
            let program = (u16::from(s[i]) << 8) | u16::from(s[i + 1]);
            let pid = (u16::from(s[i + 2] & 0x1F) << 8) | u16::from(s[i + 3]);
            if program != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            i += 4;
        }
    }

    fn pmt(&mut self, payload: &[u8]) {
        let Some(s) = Self::section(payload, 2) else { return };
        if s.len() < 13 {
            return;
        }
        let end = s.len().saturating_sub(4);
        let mut i = 12 + ((usize::from(s[10] & 0x0F) << 8) | usize::from(s[11]));
        while i + 5 <= end {
            let kind = s[i];
            let pid = (u16::from(s[i + 1] & 0x1F) << 8) | u16::from(s[i + 2]);
            let info = (usize::from(s[i + 3] & 0x0F) << 8) | usize::from(s[i + 4]);
            match kind {
                STREAM_H264 if self.video_pid.is_none() => self.video_pid = Some(pid),
                STREAM_AAC_ADTS if self.audio_pid.is_none() => self.audio_pid = Some(pid),
                STREAM_H264 | STREAM_AAC_ADTS => {}
                other if !self.unsupported.contains(&other) => self.unsupported.push(other),
                _ => {}
            }
            i += 5 + info;
        }
    }
}

fn is_packed_audio(data: &[u8]) -> bool {
    let transport = data.first() == Some(&SYNC) && data.get(PACKET) == Some(&SYNC);
    !transport && (data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0))
}

fn syncsafe(b: &[u8]) -> usize {
    b.iter().fold(0, |acc, &v| (acc << 7) | usize::from(v & 0x7F))
}

const TIMESTAMP_OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp";

// The start time (100 ns) stored in an ID3 tag by HLS packagers, and the data after the tag.
fn parse_id3(data: &[u8]) -> (Option<i64>, &[u8]) {
    if !data.starts_with(b"ID3") || data.len() < 10 {
        return (None, data);
    }
    let end = (10 + syncsafe(&data[6..10])).min(data.len());
    let (tag, body) = (&data[10..end], &data[end..]);
    let v4 = data[3] == 4;
    let mut pts = None;
    let mut i = 0;
    while i + 10 <= tag.len() {
        let size = if v4 {
            syncsafe(&tag[i + 4..i + 8])
        } else {
            u32::from_be_bytes([tag[i + 4], tag[i + 5], tag[i + 6], tag[i + 7]]) as usize
        };
        if size == 0 {
            break;
        }
        let Some(frame) = tag.get(i + 10..i + 10 + size) else { break };
        if &tag[i..i + 4] == b"PRIV"
            && let Some(nul) = frame.iter().position(|&b| b == 0)
            && &frame[..nul] == TIMESTAMP_OWNER
            && frame.len() >= nul + 9
        {
            let ticks = u64::from_be_bytes(frame[nul + 1..nul + 9].try_into().unwrap_or([0; 8])) & ((1 << 33) - 1);
            pts = Some(ticks as i64 * 1000 / 9);
        }
        i += 10 + size;
    }
    (pts, body)
}

#[derive(Debug, PartialEq)]
pub(super) struct AdtsFrame<'a> {
    pub sample_rate: u32,
    pub channels: u16,
    pub raw: &'a [u8],
}

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

// Splits a PES payload into its ADTS frames, without the headers.
pub(super) fn split_adts(data: &[u8]) -> Vec<AdtsFrame<'_>> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        if data[i] != 0xFF || data[i + 1] & 0xF0 != 0xF0 {
            i += 1;
            continue;
        }
        let header = if data[i + 1] & 1 == 0 { 9 } else { 7 };
        let rate = RATES.get(usize::from((data[i + 2] >> 2) & 0x0F)).copied();
        let channels = (u16::from(data[i + 2] & 1) << 2) | u16::from(data[i + 3] >> 6);
        let length = (usize::from(data[i + 3] & 3) << 11) | (usize::from(data[i + 4]) << 3) | usize::from(data[i + 5] >> 5);
        if length < header || i + length > data.len() {
            break;
        }
        if let Some(sample_rate) = rate {
            frames.push(AdtsFrame { sample_rate, channels, raw: &data[i + header..i + length] });
        }
        i += length;
    }
    frames
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn ts_packet(pid: u16, start: bool, counter: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![SYNC, (u8::from(start) << 6) | (pid >> 8) as u8, pid as u8];
        if payload.len() < PACKET - 4 {
            // Adaptation field padding with stuffing bytes.
            let pad = PACKET - 4 - payload.len();
            p.push(0x30 | (counter & 0x0F));
            p.push((pad - 1) as u8);
            if pad > 1 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, pad - 2));
            }
        } else {
            p.push(0x10 | (counter & 0x0F));
        }
        p.extend_from_slice(payload);
        assert_eq!(p.len(), PACKET);
        p
    }

    fn pes(stream_id: u8, pts_ticks: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5];
        v.extend_from_slice(&[
            0x21 | ((pts_ticks >> 29) & 0x0E) as u8,
            (pts_ticks >> 22) as u8,
            0x01 | ((pts_ticks >> 14) & 0xFE) as u8,
            (pts_ticks >> 7) as u8,
            0x01 | ((pts_ticks << 1) & 0xFE) as u8,
        ]);
        v.extend_from_slice(payload);
        v
    }

    fn section(table: u8, body: &[u8]) -> Vec<u8> {
        let length = body.len() + 5 + 4;
        let mut s = vec![0, table, 0xB0 | (length >> 8) as u8, length as u8, 0, 1, 0xC1, 0, 0];
        s.extend_from_slice(body);
        s.extend_from_slice(&[0, 0, 0, 0]);
        s
    }

    fn chunk(pid: u16, first: bool, counter: &mut u8, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, part) in data.chunks(PACKET - 4).enumerate() {
            out.extend(ts_packet(pid, first && i == 0, *counter, part));
            *counter = counter.wrapping_add(1);
        }
        out
    }

    // A segment with PAT, PMT, then the `video` (pts ticks, payload) and `audio` PES packets.
    pub(crate) fn segment(video: &[(u64, Vec<u8>)], audio: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut c0, mut c1, mut cv, mut ca) = (0u8, 0u8, 0u8, 0u8);
        out.extend(chunk(0, true, &mut c0, &section(0, &[0, 1, 0xF0, 0x00])));
        let mut pmt = vec![0xE1, 0x00, 0xF0, 0x00, STREAM_H264, 0xE1, 0x00, 0xF0, 0x00];
        if !audio.is_empty() {
            pmt.extend_from_slice(&[STREAM_AAC_ADTS, 0xE1, 0x01, 0xF0, 0x00]);
        }
        out.extend(chunk(0x1000, true, &mut c1, &section(2, &pmt)));
        for (ticks, data) in video {
            out.extend(chunk(0x100, true, &mut cv, &pes(0xE0, *ticks, data)));
        }
        for (ticks, data) in audio {
            out.extend(chunk(0x101, true, &mut ca, &pes(0xC0, *ticks, data)));
        }
        out
    }

    #[test]
    fn pes_times_are_read_in_100ns() {
        // 90 000 ticks = 1 second.
        let v = pes(0xE0, 90_000, &[1, 2, 3]);
        let (pts, payload) = parse_pes(&v).unwrap();
        assert_eq!(pts, Some(10_000_000));
        assert_eq!(payload, [1, 2, 3]);
        let (pts, _) = parse_pes(&pes(0xE0, 8_589_000_000, &[0])).unwrap();
        assert_eq!(pts, Some(8_589_000_000 * 1000 / 9));
        assert!(parse_pes(&[0, 0, 2, 0xE0, 0, 0, 0x80, 0x80, 5]).is_none());
    }

    #[test]
    fn demuxes_video_and_audio_streams() {
        // PAT -> PMT on 0x1000; video on 0x100, audio on 0x101.
        let mut out = Vec::new();
        let (mut c0, mut c1, mut cv, mut ca) = (0u8, 0u8, 0u8, 0u8);
        out.extend(chunk(0, true, &mut c0, &section(0, &[0, 1, 0xF0, 0x00])));
        let pmt = [0xE1, 0x00, 0xF0, 0x00, STREAM_H264, 0xE1, 0x00, 0xF0, 0x00, STREAM_AAC_ADTS, 0xE1, 0x01, 0xF0, 0x00, 0x15, 0xE1, 0x02, 0xF0, 0x00];
        out.extend(chunk(0x1000, true, &mut c1, &section(2, &pmt)));
        // A large video frame that spans several TS packets, then a second one.
        let frame1: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let frame2 = vec![9u8; 50];
        out.extend(chunk(0x100, true, &mut cv, &pes(0xE0, 90_000, &frame1)));
        out.extend(chunk(0x101, true, &mut ca, &pes(0xC0, 91_800, &[0xAA; 30])));
        out.extend(chunk(0x100, true, &mut cv, &pes(0xE0, 93_000, &frame2)));
        let mut demuxer = Demuxer::default();
        let packets = demuxer.push_segment(&out);
        let video: Vec<_> = packets.iter().filter(|p| p.kind == Kind::Video).collect();
        let audio: Vec<_> = packets.iter().filter(|p| p.kind == Kind::Audio).collect();
        assert_eq!(video.len(), 2);
        assert_eq!(video[0].data, frame1);
        assert_eq!(video[0].pts, Some(10_000_000));
        assert_eq!(video[1].data, frame2);
        assert_eq!(video[1].pts, Some(93_000 * 1000 / 9));
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].data, vec![0xAA; 30]);
        assert_eq!(demuxer.unsupported, [0x15]);
    }

    #[test]
    fn a_segment_cut_before_the_first_packet_is_resynchronised() {
        let mut out = vec![0u8; 37];
        let (mut c0, mut c1, mut cv) = (0u8, 0u8, 0u8);
        out.extend(chunk(0, true, &mut c0, &section(0, &[0, 1, 0xF0, 0x00])));
        let pmt = [0xE1, 0x00, 0xF0, 0x00, STREAM_H264, 0xE1, 0x00, 0xF0, 0x00];
        out.extend(chunk(0x1000, true, &mut c1, &section(2, &pmt)));
        out.extend(chunk(0x100, true, &mut cv, &pes(0xE0, 9_000, &[7; 10])));
        let packets = Demuxer::default().push_segment(&out);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, vec![7; 10]);
    }

    fn adts(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() + 7;
        let mut frame = vec![0xFF, 0xF1, 0x50, 0x80 | ((len >> 11) & 3) as u8, (len >> 3) as u8, (((len & 7) << 5) | 0x1F) as u8, 0xFC];
        frame.extend_from_slice(payload);
        frame
    }

    // YouTube's audio renditions are raw ADTS behind an ID3 tag carrying the start time.
    #[test]
    fn packed_audio_segments_become_one_timed_packet() {
        let owner = [TIMESTAMP_OWNER, &[0]].concat();
        let ticks: u64 = 90_000 * 12; // 12 s
        let mut priv_body = owner.clone();
        priv_body.extend_from_slice(&ticks.to_be_bytes());
        let mut tag = Vec::new();
        tag.extend_from_slice(b"PRIV");
        tag.extend_from_slice(&(priv_body.len() as u32).to_be_bytes());
        tag.extend_from_slice(&[0, 0]);
        tag.extend_from_slice(&priv_body);
        let size = tag.len();
        let mut data = vec![b'I', b'D', b'3', 3, 0, 0, 0, 0, ((size >> 7) & 0x7F) as u8, (size & 0x7F) as u8];
        data.extend_from_slice(&tag);
        data.extend(adts(&[1, 2, 3]));
        data.extend(adts(&[4, 5]));
        let mut demuxer = Demuxer::default();
        let packets = demuxer.push_segment(&data);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].kind, Kind::Audio);
        assert_eq!(packets[0].pts, Some(12 * 10_000_000));
        assert_eq!(split_adts(&packets[0].data).len(), 2);
        assert!(demuxer.has_audio());
        // Without a tag the next segment continues where the previous one ended.
        let plain: Vec<u8> = [adts(&[1]), adts(&[2])].concat();
        let first = demuxer.push_segment(&plain)[0].pts.unwrap();
        let second = demuxer.push_segment(&plain)[0].pts.unwrap();
        // Each frame is rounded down to whole ticks of 100 ns.
        assert!((second - first - 2 * 1024 * 10_000_000 / 44_100).abs() <= 2);
    }

    #[test]
    fn splits_adts_frames() {
        // Two frames of 7 + 5 bytes: AAC-LC, 44.1 kHz (index 4), stereo, no CRC.
        let header = |len: usize| -> Vec<u8> {
            vec![0xFF, 0xF1, 0x50, 0x80 | ((len >> 11) & 3) as u8, (len >> 3) as u8, (((len & 7) << 5) | 0x1F) as u8, 0xFC]
        };
        let mut data = header(12);
        data.extend_from_slice(&[1, 2, 3, 4, 5]);
        data.extend(header(10));
        data.extend_from_slice(&[6, 7, 8]);
        let frames = split_adts(&data);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], AdtsFrame { sample_rate: 44100, channels: 2, raw: &[1, 2, 3, 4, 5] });
        assert_eq!(frames[1].raw, [6, 7, 8]);
        // A truncated trailing frame is dropped.
        assert_eq!(split_adts(&data[..data.len() - 1]).len(), 1);
    }
}
