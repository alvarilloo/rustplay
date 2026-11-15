// Fragmented MP4 (CMAF) demuxer for HLS version 6+ playlists: an initialization segment
// (#EXT-X-MAP, with the `moov` box) followed by media segments made of `moof`/`mdat` pairs. It
// yields the same packets as the transport stream demuxer: H.264 as Annex B access units (with
// the parameter sets in front of every keyframe, so a quality switch can change the resolution)
// and AAC as ADTS frames, one packet per fragment.

use super::ts::{Kind, Packet};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

struct BoxRef<'a> {
    kind: [u8; 4],
    // Offset of the box inside the slice that was parsed.
    start: usize,
    body: &'a [u8],
}

// The boxes laid one after another in `data`; a truncated last box ends the list.
fn boxes(data: &[u8]) -> Vec<BoxRef<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size32 = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let kind = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
        let (header, size) = match size32 {
            0 => (8, data.len() - pos),
            1 => {
                let Some(big) = data.get(pos + 8..pos + 16) else { break };
                (16, u64::from_be_bytes(big.try_into().unwrap_or([0; 8])) as usize)
            }
            n => (8, n as usize),
        };
        if size < header || pos + size > data.len() {
            break;
        }
        out.push(BoxRef { kind, start: pos, body: &data[pos + header..pos + size] });
        pos += size;
    }
    out
}

fn child<'a>(parent: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(parent).into_iter().find(|b| &b.kind == kind).map(|b| b.body)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

#[derive(Debug, Clone, PartialEq)]
struct AacConfig {
    // 2-bit ADTS profile (audio object type minus one).
    profile: u8,
    rate_index: u8,
    sample_rate: u32,
    channels: u8,
}

const AAC_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(v)
    }
}

// AudioSpecificConfig of an AAC stream.
fn parse_asc(asc: &[u8]) -> Option<AacConfig> {
    let mut bits = Bits { data: asc, pos: 0 };
    let mut object = bits.read(5)?;
    if object == 31 {
        object = 32 + bits.read(6)?;
    }
    let read_rate = |bits: &mut Bits<'_>| -> Option<(u8, u32)> {
        let index = bits.read(4)?;
        if index == 15 {
            Some((15, bits.read(24)?))
        } else {
            Some((index as u8, *AAC_RATES.get(index as usize)?))
        }
    };
    let (mut rate_index, mut sample_rate) = read_rate(&mut bits)?;
    let channels = bits.read(4)? as u8;
    // Explicit SBR/PS signalling: the core codec follows; ADTS carries the core's rate.
    if object == 5 || object == 29 {
        let _ = read_rate(&mut bits)?;
        object = bits.read(5)?;
        if object == 31 {
            object = 32 + bits.read(6)?;
        }
    }
    if !(1..=4).contains(&object) || rate_index == 15 {
        // ADTS cannot express other object types or explicit rates; fall back to the nearest index.
        let nearest = AAC_RATES.iter().position(|&r| r == sample_rate)?;
        rate_index = nearest as u8;
        sample_rate = AAC_RATES[nearest];
        if !(1..=4).contains(&object) {
            return None;
        }
    }
    Some(AacConfig { profile: (object - 1) as u8, rate_index, sample_rate, channels })
}

// Length of an MPEG-4 descriptor header: (tag, body length, header size).
fn descriptor(data: &[u8]) -> Option<(u8, usize, usize)> {
    let tag = *data.first()?;
    let mut len = 0usize;
    for i in 1..=4 {
        let b = *data.get(i)?;
        len = (len << 7) | usize::from(b & 0x7F);
        if b & 0x80 == 0 {
            return Some((tag, len, i + 1));
        }
    }
    None
}

// The AudioSpecificConfig inside an `esds` box body.
fn esds_config(body: &[u8]) -> Option<&[u8]> {
    // Skip version and flags, then walk ES_Descriptor > DecoderConfigDescriptor > DecoderSpecificInfo.
    let mut cur = body.get(4..)?;
    loop {
        let (tag, len, header) = descriptor(cur)?;
        let inner = cur.get(header..header + len.min(cur.len() - header))?;
        match tag {
            0x03 => {
                // ES_ID (2), flags (1), optional dependsOn (2), URL, OCR (2).
                let flags = *inner.get(2)?;
                let mut skip = 3;
                if flags & 0x80 != 0 {
                    skip += 2;
                }
                if flags & 0x40 != 0 {
                    skip += 1 + usize::from(*inner.get(skip)?);
                }
                if flags & 0x20 != 0 {
                    skip += 2;
                }
                cur = inner.get(skip..)?;
            }
            0x04 => cur = inner.get(13..)?,
            0x05 => return Some(inner),
            _ => cur = cur.get(header + len..)?,
        }
    }
}

struct VideoConfig {
    nal_length: usize,
    // SPS and PPS as Annex B.
    parameter_sets: Vec<u8>,
}

fn parse_avcc(body: &[u8]) -> Option<VideoConfig> {
    if body.len() < 7 || body[0] != 1 {
        return None;
    }
    let nal_length = usize::from(body[4] & 3) + 1;
    let mut pos = 6;
    let mut sets = Vec::new();
    let count = usize::from(body[5] & 0x1F);
    let take = |n: usize, pos: &mut usize, sets: &mut Vec<u8>| -> Option<()> {
        for _ in 0..n {
            let len = usize::from(u16::from_be_bytes(body.get(*pos..*pos + 2)?.try_into().ok()?));
            sets.extend_from_slice(&START_CODE);
            sets.extend_from_slice(body.get(*pos + 2..*pos + 2 + len)?);
            *pos += 2 + len;
        }
        Some(())
    };
    take(count, &mut pos, &mut sets)?;
    let pps = usize::from(*body.get(pos)?);
    pos += 1;
    take(pps, &mut pos, &mut sets)?;
    (!sets.is_empty()).then_some(VideoConfig { nal_length, parameter_sets: sets })
}

enum Codec {
    Video(VideoConfig),
    Audio(AacConfig),
}

struct Track {
    id: u32,
    timescale: u32,
    codec: Codec,
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
    // Where the next fragment continues when it carries no explicit decode time.
    next_dts: u64,
}

#[derive(Default)]
pub(super) struct Mp4Demuxer {
    tracks: Vec<Track>,
}

impl Mp4Demuxer {
    // Reads the tracks out of an initialization segment.
    pub fn from_init(init: &[u8]) -> Result<Self, String> {
        let moov = child(init, b"moov").ok_or("el segmento de inicialización no tiene moov")?;
        let mut tracks = Vec::new();
        for trak in boxes(moov).into_iter().filter(|b| &b.kind == b"trak") {
            if let Some(track) = Self::track(trak.body)? {
                tracks.push(track);
            }
        }
        if let Some(mvex) = child(moov, b"mvex") {
            for trex in boxes(mvex).into_iter().filter(|b| &b.kind == b"trex") {
                let b = trex.body;
                let (Some(id), Some(duration), Some(size), Some(flags)) = (be32(b, 4), be32(b, 12), be32(b, 16), be32(b, 20)) else { continue };
                if let Some(t) = tracks.iter_mut().find(|t| t.id == id) {
                    (t.default_duration, t.default_size, t.default_flags) = (duration, size, flags);
                }
            }
        }
        if tracks.is_empty() {
            return Err("el vídeo fMP4 no tiene pistas H.264 ni AAC".into());
        }
        Ok(Self { tracks })
    }

    fn track(trak: &[u8]) -> Result<Option<Track>, String> {
        let version_at = |b: &[u8], v0: usize, v1: usize| be32(b, if b.first() == Some(&1) { v1 } else { v0 });
        let Some(tkhd) = child(trak, b"tkhd") else { return Ok(None) };
        let Some(id) = version_at(tkhd, 12, 20) else { return Ok(None) };
        let Some(mdia) = child(trak, b"mdia") else { return Ok(None) };
        let Some(timescale) = child(mdia, b"mdhd").and_then(|m| version_at(m, 12, 20)) else { return Ok(None) };
        let handler = child(mdia, b"hdlr").and_then(|h| h.get(8..12)).unwrap_or_default();
        let Some(stsd) = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).and_then(|s| child(s, b"stsd")) else { return Ok(None) };
        let Some(entry) = boxes(stsd.get(8..).unwrap_or_default()).into_iter().next() else { return Ok(None) };
        let codec = match (handler, &entry.kind) {
            (b"vide", b"avc1" | b"avc3") => {
                let avcc = child(entry.body.get(78..).unwrap_or_default(), b"avcC").ok_or("falta avcC")?;
                Codec::Video(parse_avcc(avcc).ok_or("avcC no válido")?)
            }
            (b"soun", b"mp4a") => {
                let esds = child(entry.body.get(28..).unwrap_or_default(), b"esds").ok_or("falta esds")?;
                let asc = esds_config(esds).ok_or("esds no válido")?;
                match parse_asc(asc) {
                    Some(c) => Codec::Audio(c),
                    None => return Err("el audio AAC no es AAC-LC".into()),
                }
            }
            // Subtitles, other codecs: ignored (a stream without a usable track is reported later).
            _ => return Ok(None),
        };
        Ok(Some(Track { id, timescale: timescale.max(1), codec, default_duration: 0, default_size: 0, default_flags: 0, next_dts: 0 }))
    }

    pub fn has_audio(&self) -> bool {
        self.tracks.iter().any(|t| matches!(t.codec, Codec::Audio(_)))
    }

    // All the fragments of one media segment, in stream order.
    pub fn push_segment(&mut self, data: &[u8]) -> Vec<Packet> {
        let mut out = Vec::new();
        for moof in boxes(data).into_iter().filter(|b| &b.kind == b"moof") {
            for traf in boxes(moof.body).into_iter().filter(|b| &b.kind == b"traf") {
                self.fragment(data, moof.start, traf.body, &mut out);
            }
        }
        out
    }

    fn fragment(&mut self, data: &[u8], moof_start: usize, traf: &[u8], out: &mut Vec<Packet>) {
        let Some(tfhd) = child(traf, b"tfhd") else { return };
        let Some(flags) = be32(tfhd, 0).map(|v| v & 0x00FF_FFFF) else { return };
        let Some(id) = be32(tfhd, 4) else { return };
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) else { return };
        let mut at = 8;
        let take32 = |wanted: u32, at: &mut usize| -> Option<u32> {
            if flags & wanted == 0 {
                return None;
            }
            let v = be32(tfhd, *at);
            *at += 4;
            v
        };
        let mut base = moof_start as u64;
        if flags & 0x1 != 0 {
            base = be64(tfhd, at).unwrap_or(base);
            at += 8;
        }
        let _sample_description = take32(0x2, &mut at);
        let default_duration = take32(0x8, &mut at).unwrap_or(track.default_duration);
        let default_size = take32(0x10, &mut at).unwrap_or(track.default_size);
        let _default_flags = take32(0x20, &mut at);
        let mut dts = child(traf, b"tfdt")
            .and_then(|t| if t.first() == Some(&1) { be64(t, 4) } else { be32(t, 4).map(u64::from) })
            .unwrap_or(track.next_dts);
        let mut cursor = base;
        let mut audio = Vec::new();
        let mut audio_pts = None;
        for trun in boxes(traf).into_iter().filter(|b| &b.kind == b"trun") {
            let b = trun.body;
            let (Some(head), Some(count)) = (be32(b, 0), be32(b, 4)) else { continue };
            let (version, tflags) = ((head >> 24) as u8, head & 0x00FF_FFFF);
            let mut p = 8;
            if tflags & 0x1 != 0 {
                let Some(offset) = be32(b, p) else { continue };
                cursor = base.wrapping_add_signed(i64::from(offset as i32));
                p += 4;
            }
            if tflags & 0x4 != 0 {
                p += 4;
            }
            for _ in 0..count {
                let mut field = |wanted: u32, default: u32| -> Option<u32> {
                    if tflags & wanted == 0 {
                        return Some(default);
                    }
                    let v = be32(b, p);
                    p += 4;
                    v
                };
                let (Some(duration), Some(size), Some(_flags), Some(cto)) =
                    (field(0x100, default_duration), field(0x200, default_size), field(0x400, 0), field(0x800, 0))
                else {
                    break;
                };
                let cto = if version == 0 { i64::from(cto) } else { i64::from(cto as i32) };
                let (start, end) = (cursor as usize, cursor as usize + size as usize);
                let Some(sample) = data.get(start..end) else { break };
                cursor += u64::from(size);
                let pts_ticks = dts as i64 + cto;
                let pts = (i128::from(pts_ticks) * 10_000_000 / i128::from(track.timescale)) as i64;
                match &track.codec {
                    Codec::Video(config) => {
                        if let Some(access_unit) = annex_b(sample, config) {
                            out.push(Packet { kind: Kind::Video, pts: Some(pts), data: access_unit });
                        }
                    }
                    Codec::Audio(config) => {
                        audio_pts.get_or_insert(pts);
                        add_adts(&mut audio, config, sample);
                    }
                }
                dts += u64::from(duration);
            }
        }
        track.next_dts = dts;
        if let Some(pts) = audio_pts
            && !audio.is_empty()
        {
            out.push(Packet { kind: Kind::Audio, pts: Some(pts), data: audio });
        }
    }
}

fn add_adts(out: &mut Vec<u8>, config: &AacConfig, raw: &[u8]) {
    let len = raw.len() + 7;
    out.extend_from_slice(&[
        0xFF,
        0xF1,
        (config.profile << 6) | (config.rate_index << 2) | ((config.channels >> 2) & 1),
        ((config.channels & 3) << 6) | ((len >> 11) & 3) as u8,
        (len >> 3) as u8,
        (((len & 7) << 5) | 0x1F) as u8,
        0xFC,
    ]);
    out.extend_from_slice(raw);
}

// A length-prefixed sample as an Annex B access unit. Keyframes get the parameter sets in front
// unless they already carry them.
fn annex_b(sample: &[u8], config: &VideoConfig) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(sample.len() + 64);
    let (mut key, mut has_sps) = (false, false);
    let mut pos = 0;
    while pos + config.nal_length <= sample.len() {
        let len = sample[pos..pos + config.nal_length].iter().fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        pos += config.nal_length;
        let nal = sample.get(pos..pos + len)?;
        pos += len;
        match nal.first().map(|b| b & 0x1F) {
            Some(5) => key = true,
            Some(7) => has_sps = true,
            _ => {}
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    if out.is_empty() {
        return None;
    }
    if key && !has_sps {
        let mut with_sets = Vec::with_capacity(out.len() + config.parameter_sets.len());
        with_sets.extend_from_slice(&config.parameter_sets);
        with_sets.extend_from_slice(&out);
        return Some(with_sets);
    }
    Some(out)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::media::player::native::ts::split_adts;

    pub(crate) fn mk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vec![version, (flags >> 16) as u8, (flags >> 8) as u8, flags as u8];
        b.extend_from_slice(body);
        mk(kind, &b)
    }

    const SPS: [u8; 4] = [0x67, 0x42, 0xC0, 0x1E];
    const PPS: [u8; 2] = [0x68, 0xCE];

    fn avc1_entry(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
        let mut body = vec![0u8; 78];
        body.extend(mk(b"avcC", &avcc));
        mk(b"avc1", &body)
    }

    // AAC-LC, 44.1 kHz (index 4), stereo: 0x12 0x10.
    const ASC: [u8; 2] = [0x12, 0x10];

    fn mp4a_entry() -> Vec<u8> {
        let mut es = vec![0x03, 25, 0, 2, 0, 0x04, 17, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05, 2];
        es.extend_from_slice(&ASC);
        es.extend_from_slice(&[0x06, 1, 2]);
        let mut body = vec![0u8; 28];
        body.extend(full(b"esds", 0, 0, &es));
        mk(b"mp4a", &body)
    }

    fn trak(id: u32, timescale: u32, handler: &[u8; 4], entry: &[u8]) -> Vec<u8> {
        // Creation and modification times, then the track id.
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&id.to_be_bytes());
        tkhd.extend_from_slice(&[0; 68]);
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0; 8]);
        // Pre-defined, then the handler type.
        let mut hdlr = vec![0u8; 4];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0; 13]);
        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend_from_slice(entry);
        let stbl = mk(b"stbl", &full(b"stsd", 0, 0, &stsd));
        let minf = mk(b"minf", &stbl);
        let mut mdia = full(b"mdhd", 0, 0, &mdhd);
        mdia.extend(full(b"hdlr", 0, 0, &hdlr));
        mdia.extend(minf);
        let mut t = full(b"tkhd", 0, 3, &tkhd);
        t.extend(mk(b"mdia", &mdia));
        mk(b"trak", &t)
    }

    pub(crate) fn init_segment() -> Vec<u8> {
        init_segment_with(&SPS, &PPS)
    }

    // ftyp + moov with a video track (id 1, 90 kHz, the given parameter sets) and an AAC-LC audio
    // track (id 2, 44.1 kHz stereo).
    pub(crate) fn init_segment_with(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut moov = trak(1, 90_000, b"vide", &avc1_entry(sps, pps));
        moov.extend(trak(2, 44_100, b"soun", &mp4a_entry()));
        let mut trex = 1u32.to_be_bytes().to_vec();
        trex.extend_from_slice(&[0; 4]);
        trex.extend_from_slice(&3000u32.to_be_bytes());
        trex.extend_from_slice(&[0; 8]);
        moov.extend(mk(b"mvex", &full(b"trex", 0, 0, &trex)));
        let mut file = mk(b"ftyp", b"iso6\0\0\0\0iso6cmfc");
        file.extend(mk(b"moov", &moov));
        file
    }

    // (data, duration, composition offset) of a sample; a run is (track id, decode time, samples).
    type Sample = (Vec<u8>, u32, i32);
    pub(crate) type Run = (u32, u64, Vec<Sample>);

    // styp + moof + mdat for the runs (track id, decode time, samples as (data, duration,
    // composition offset)); data offsets are relative to the moof, as CMAF requires.
    pub(crate) fn fragment(runs: &[Run]) -> Vec<u8> {
        // Two passes: the moof size must be known to write the data offsets.
        let build = |offset_base: usize| -> (Vec<u8>, Vec<u8>) {
            let mut trafs = Vec::new();
            let mut datas = Vec::new();
            for (id, base_time, samples) in runs {
                let tfhd = id.to_be_bytes().to_vec();
                let mut tfdt = Vec::new();
                tfdt.extend_from_slice(&base_time.to_be_bytes());
                let mut trun = Vec::new();
                trun.extend_from_slice(&(samples.len() as u32).to_be_bytes());
                trun.extend_from_slice(&((offset_base + datas.len()) as u32).to_be_bytes());
                for (data, duration, cto) in samples {
                    trun.extend_from_slice(&duration.to_be_bytes());
                    trun.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    trun.extend_from_slice(&cto.to_be_bytes());
                    datas.extend_from_slice(data);
                }
                let mut traf = full(b"tfhd", 0, 0x20000, &tfhd);
                traf.extend(full(b"tfdt", 1, 0, &tfdt));
                traf.extend(full(b"trun", 1, 0x1 | 0x100 | 0x200 | 0x800, &trun));
                trafs.extend(mk(b"traf", &traf));
            }
            (trafs, datas)
        };
        let moof_size = 8 + mk(b"mfhd", &[0; 8]).len() + build(0).0.len();
        // The data starts after the moof and the 8-byte mdat header.
        let (trafs, datas) = build(moof_size + 8);
        let mut moof_body = full(b"mfhd", 0, 0, &1u32.to_be_bytes());
        moof_body.extend(trafs);
        let mut out = mk(b"styp", b"msdh");
        let moof = mk(b"moof", &moof_body);
        assert_eq!(moof.len(), moof_size);
        out.extend(moof);
        out.extend(mk(b"mdat", &datas));
        out
    }

    fn nal(kind: u8, len: usize) -> Vec<u8> {
        let mut v = (len as u32 + 1).to_be_bytes().to_vec();
        v.push(kind);
        v.extend(std::iter::repeat_n(kind, len));
        v
    }

    #[test]
    fn reads_the_tracks_of_an_init_segment() {
        let d = Mp4Demuxer::from_init(&init_segment()).unwrap();
        assert!(d.has_audio());
        assert_eq!(d.tracks.len(), 2);
        assert_eq!((d.tracks[0].id, d.tracks[0].timescale, d.tracks[0].default_duration), (1, 90_000, 3000));
        let Codec::Video(v) = &d.tracks[0].codec else { panic!("video first") };
        assert_eq!(v.nal_length, 4);
        assert_eq!(v.parameter_sets, [&START_CODE[..], &SPS, &START_CODE, &PPS].concat());
        let Codec::Audio(a) = &d.tracks[1].codec else { panic!("audio second") };
        assert_eq!(*a, AacConfig { profile: 1, rate_index: 4, sample_rate: 44_100, channels: 2 });
        assert!(Mp4Demuxer::from_init(&mk(b"ftyp", b"iso6")).is_err());
    }

    #[test]
    fn audio_specific_config_and_descriptors() {
        assert_eq!(parse_asc(&ASC).unwrap().sample_rate, 44_100);
        // 48 kHz mono AAC-LC.
        assert_eq!(parse_asc(&[0x11, 0x88]), Some(AacConfig { profile: 1, rate_index: 3, sample_rate: 48_000, channels: 1 }));
        // HE-AAC signalled explicitly (object 5, core 24 kHz, extension 48 kHz): ADTS describes the core.
        let he = parse_asc(&[0x2B, 0x11, 0x88]).unwrap();
        assert_eq!((he.profile, he.sample_rate), (1, 24_000));
        // Multi-byte descriptor lengths.
        assert_eq!(descriptor(&[0x03, 0x80, 0x80, 0x80, 0x19]), Some((3, 25, 5)));
        assert_eq!(descriptor(&[0x05, 0x02]), Some((5, 2, 2)));
    }

    #[test]
    fn converts_video_samples_to_annex_b_and_times_them() {
        let mut d = Mp4Demuxer::from_init(&init_segment()).unwrap();
        // Keyframe (IDR) then a P frame and a B frame displayed before it: decode order I P B.
        let key = [nal(6, 3), nal(5, 10)].concat();
        let p = nal(1, 6);
        let b = nal(1, 4);
        // A trun-less default duration would come from trex; here every sample says 3000.
        let seg = fragment(&[(1, 90_000 * 10, vec![(key.clone(), 3000, 6000), (p.clone(), 3000, 9000), (b.clone(), 3000, 3000)])]);
        let packets = d.push_segment(&seg);
        assert_eq!(packets.len(), 3);
        assert!(packets.iter().all(|p| p.kind == Kind::Video));
        // Keyframe: parameter sets first, then the SEI and IDR with start codes.
        let expected_key = [&START_CODE[..], &SPS, &START_CODE, &PPS, &START_CODE, &[6, 6, 6, 6], &START_CODE, &[5; 11]].concat();
        assert_eq!(packets[0].data, expected_key);
        assert_eq!(packets[1].data, [&START_CODE[..], &[1; 7]].concat());
        // 90 kHz ticks to 100 ns: base 10 s, plus composition offsets.
        assert_eq!(packets[0].pts, Some(10 * 10_000_000 + 6000 * 10_000_000 / 90_000));
        assert_eq!(packets[1].pts, Some(10 * 10_000_000 + (3000 + 9000) * 10_000_000 / 90_000));
        assert_eq!(packets[2].pts, Some(10 * 10_000_000 + (6000 + 3000) * 10_000_000 / 90_000));
        // The next fragment without a decode time of its own would continue; with one it uses it.
        assert_eq!(d.tracks[0].next_dts, 90_000 * 10 + 9000);
    }

    #[test]
    fn keyframes_that_carry_their_parameter_sets_are_not_doubled() {
        let mut d = Mp4Demuxer::from_init(&init_segment()).unwrap();
        let key = [nal(7, 3), nal(8, 2), nal(5, 4)].concat();
        let packets = d.push_segment(&fragment(&[(1, 0, vec![(key, 3000, 0)])]));
        let sps_count = packets[0].data.windows(5).filter(|w| *w == [0, 0, 0, 1, 7]).count();
        assert_eq!(sps_count, 1);
    }

    #[test]
    fn wraps_audio_samples_in_adts_and_keeps_the_time() {
        let mut d = Mp4Demuxer::from_init(&init_segment()).unwrap();
        let seg = fragment(&[
            (1, 0, vec![(nal(5, 8), 3000, 0)]),
            (2, 44_100 * 2, vec![(vec![1, 2, 3], 1024, 0), (vec![4, 5], 1024, 0), (vec![6; 9], 1024, 0)]),
        ]);
        let packets = d.push_segment(&seg);
        let audio: Vec<_> = packets.iter().filter(|p| p.kind == Kind::Audio).collect();
        assert_eq!(audio.len(), 1, "one packet per fragment");
        assert_eq!(audio[0].pts, Some(2 * 10_000_000));
        let frames = split_adts(&audio[0].data);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].raw, [1, 2, 3]);
        assert_eq!(frames[2].raw, [6; 9]);
        assert_eq!((frames[0].sample_rate, frames[0].channels), (44_100, 2));
        assert_eq!(packets.iter().filter(|p| p.kind == Kind::Video).count(), 1);
    }

    #[test]
    fn tolerates_truncated_and_foreign_data() {
        let mut d = Mp4Demuxer::from_init(&init_segment()).unwrap();
        let seg = fragment(&[(1, 0, vec![(nal(5, 8), 3000, 0), (nal(1, 8), 3000, 0)])]);
        // Cut inside the first sample: nothing usable, and no panic.
        assert!(d.push_segment(&seg[..seg.len() - 20]).is_empty());
        assert!(d.push_segment(&[]).is_empty());
        assert!(d.push_segment(&[0xFF; 40]).is_empty());
        // Unknown track ids are skipped.
        assert!(d.push_segment(&fragment(&[(9, 0, vec![(nal(5, 8), 3000, 0)])])).is_empty());
    }

    #[test]
    fn handles_64_bit_box_sizes() {
        let inner = mk(b"free", &[1, 2, 3]);
        let mut big = 1u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"wide");
        big.extend_from_slice(&((inner.len() + 16) as u64).to_be_bytes());
        big.extend_from_slice(&inner);
        let found = boxes(&big);
        assert_eq!(found.len(), 1);
        assert_eq!(boxes(found[0].body)[0].body, [1, 2, 3]);
    }
}
