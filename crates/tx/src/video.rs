use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustplay_core::{Error, Result};
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::clock;

pub const HEADER_LEN: usize = 128;
pub const TAG_LEN: usize = 16;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CAPTURE_AGE: Duration = Duration::from_secs(30);

const KIND_VIDEO: u8 = 0x00;
const KIND_CODEC: u8 = 0x01;
const KIND_HEARTBEAT: u8 = 0x02;
const FLAG_IDR: u8 = 0x10;

#[derive(Default)]
pub struct MirrorStats {
    pub captured: AtomicU64,
    pub repeated: AtomicU64,
    pub encoded: AtomicU64,
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
    pub keyframes: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_micros: AtomicU64,
    pub encode_max_micros: AtomicU64,
}

impl MirrorStats {
    pub fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

// Layout of APScreenProtocolHeader as sent by doubletake: [0:4] payload size (LE, tag included),
// [4] kind, [5] 0x10 on IDR, [6:8] option, [8:16] timestamp (LE 32.32), [40:48] timeline (0 for NTP).
pub fn video_header(out: &mut [u8], payload_len: usize, keyframe: bool, timestamp: u64) {
    out[..HEADER_LEN].fill(0);
    out[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    out[4] = KIND_VIDEO;
    out[5] = if keyframe { FLAG_IDR } else { 0 };
    out[8..16].copy_from_slice(&timestamp.to_le_bytes());
}

// The codec packet describes the encoded raster in the three CGRect slots (size only, zero origin).
pub fn codec_header(out: &mut [u8], payload_len: usize, timestamp: u64, width: u32, height: u32) {
    out[..HEADER_LEN].fill(0);
    out[0..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    out[4] = KIND_CODEC;
    out[6] = 0x16;
    out[7] = 0x01;
    out[8..16].copy_from_slice(&timestamp.to_le_bytes());
    let (w, h) = (width as f32, height as f32);
    for (w_at, h_at) in [(16, 20), (40, 44), (56, 60)] {
        out[w_at..w_at + 4].copy_from_slice(&w.to_le_bytes());
        out[h_at..h_at + 4].copy_from_slice(&h.to_le_bytes());
    }
}

pub fn heartbeat_header(out: &mut [u8]) {
    out[..HEADER_LEN].fill(0);
    out[4] = KIND_HEARTBEAT;
    out[6] = 0x1e;
}

// AVCDecoderConfigurationRecord followed by the 4-byte trailer doubletake appends.
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + 2 + sps.len() + 1 + 2 + pps.len() + 4);
    out.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xFF, 0xE1]);
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out.extend_from_slice(&[0x02, 0, 0, 0]);
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    overrun: bool,
}

impl BitReader<'_> {
    fn bit(&mut self) -> u32 {
        let Some(byte) = self.data.get(self.pos >> 3) else {
            self.overrun = true;
            return 0;
        };
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        u32::from(bit)
    }

    fn bits(&mut self, count: u32) -> u32 {
        (0..count).fold(0, |acc, _| (acc << 1) | self.bit())
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            if self.overrun || zeros > 31 {
                self.overrun = true;
                return 0;
            }
            zeros += 1;
        }
        if zeros == 0 { 0 } else { (1u32 << zeros) - 1 + self.bits(zeros) }
    }

    fn se(&mut self) -> i32 {
        let k = self.ue();
        if k & 1 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) }
    }
}

fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for (i, &byte) in data.iter().enumerate() {
        if zeros >= 2 && byte == 3 && data.get(i + 1).is_some_and(|&next| next <= 3) {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

// Coded picture size from a raw SPS (NAL header included, no start code), 4:2:0 cropping assumed.
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    if sps.len() < 4 || sps[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = strip_emulation_prevention(&sps[1..]);
    let mut r = BitReader { data: &rbsp, pos: 0, overrun: false };
    let profile = r.bits(8);
    r.bits(8);
    r.bits(8);
    r.ue();
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue();
        if chroma == 3 {
            r.bit();
        }
        r.ue();
        r.ue();
        r.bit();
        if r.bit() == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.bit() == 1 {
                    let size = if i >= 6 { 64 } else { 16 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se() + 256).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue();
    match r.ue() {
        0 => {
            r.ue();
        }
        1 => {
            r.bit();
            r.se();
            r.se();
            for _ in 0..r.ue() {
                r.se();
            }
        }
        _ => {}
    }
    r.ue();
    r.bit();
    let width_mbs = r.ue() + 1;
    let height_units = r.ue() + 1;
    let frame_mbs_only = r.bit();
    if frame_mbs_only == 0 {
        r.bit();
    }
    r.bit();
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if r.bit() == 1 {
        left = r.ue();
        right = r.ue();
        top = r.ue();
        bottom = r.ue();
    }
    if r.overrun {
        return None;
    }
    let crop_y = 2 * (2 - frame_mbs_only);
    let width = (width_mbs * 16).checked_sub((left + right) * 2)?;
    let height = ((2 - frame_mbs_only) * height_units * 16).checked_sub((top + bottom) * crop_y)?;
    (width > 0 && height > 0).then_some((width, height))
}

// Position and length of the next start code at or after `from`.
fn next_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 2 < data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(if i > from && data[i - 1] == 0 { (i - 1, 4) } else { (i, 3) });
        } else {
            i += 1;
        }
    }
    None
}

// NAL units of an Annex B stream, without start codes or trailing zero bytes.
pub fn split_annexb(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut cursor = next_start_code(data, 0);
    std::iter::from_fn(move || {
        let (pos, len) = cursor?;
        let start = pos + len;
        let next = next_start_code(data, start);
        let end = next.map_or(data.len(), |(p, _)| p);
        cursor = next;
        let mut nal = &data[start..end];
        while nal.last() == Some(&0) {
            nal = &nal[..nal.len() - 1];
        }
        Some(nal)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub avcc: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

// Annex B access unit to length-prefixed (AVCC) VCL data. Parameter sets are remembered, access
// unit delimiters, SEI and anything else are dropped, as doubletake does.
#[derive(Default)]
struct ParamSets {
    sps: Vec<u8>,
    pps: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertedUnit {
    pub keyframe: bool,
    pub vcl_nals: usize,
}

fn convert(annexb: &[u8], out: &mut Vec<u8>, params: &mut ParamSets) -> ConvertedUnit {
    let mut unit = ConvertedUnit { keyframe: false, vcl_nals: 0 };
    for nal in split_annexb(annexb) {
        let Some(&header) = nal.first() else { continue };
        match header & 0x1F {
            7 => {
                params.sps.clear();
                params.sps.extend_from_slice(nal);
            }
            8 => {
                params.pps.clear();
                params.pps.extend_from_slice(nal);
            }
            kind @ (1..=5) => {
                unit.keyframe |= kind == 5;
                unit.vcl_nals += 1;
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    unit
}

// Converts an Annex B access unit into `out` (after reserving the header space) and decides when
// the decoder configuration must be advertised. The configuration is sent before the first IDR
// and again only when the SPS or PPS change.
pub struct UnitBuilder {
    params: ParamSets,
    sent: ParamSets,
    primed: bool,
    fallback: (u32, u32),
}

pub struct BuiltUnit {
    pub keyframe: bool,
    pub codec: Option<CodecConfig>,
}

impl UnitBuilder {
    pub fn new(fallback_size: (u32, u32)) -> Self {
        Self { params: ParamSets::default(), sent: ParamSets::default(), primed: false, fallback: fallback_size }
    }

    // A unit carrying the configuration was lost: advertise it again with the next IDR.
    pub fn forget_sent(&mut self) {
        self.primed = false;
        self.sent.sps.clear();
        self.sent.pps.clear();
    }

    pub fn seed(&mut self, sps: &[u8], pps: &[u8]) {
        if self.params.sps.is_empty() {
            self.params.sps.extend_from_slice(sps);
        }
        if self.params.pps.is_empty() {
            self.params.pps.extend_from_slice(pps);
        }
    }

    pub fn build(&mut self, annexb: &[u8], out: &mut Vec<u8>) -> Option<BuiltUnit> {
        out.clear();
        out.resize(HEADER_LEN, 0);
        let unit = convert(annexb, out, &mut self.params);
        if unit.vcl_nals == 0 {
            return None;
        }
        let ready = !self.params.sps.is_empty() && self.params.sps.len() >= 4 && !self.params.pps.is_empty();
        if !self.primed && !(unit.keyframe && ready) {
            return None;
        }
        let mut codec = None;
        let changed = self.params.sps != self.sent.sps || self.params.pps != self.sent.pps;
        if unit.keyframe && ready && (!self.primed || changed) {
            let (width, height) = sps_dimensions(&self.params.sps).unwrap_or(self.fallback);
            codec = Some(CodecConfig { avcc: build_avcc(&self.params.sps, &self.params.pps), width, height });
            self.sent.sps.clone_from(&self.params.sps);
            self.sent.pps.clone_from(&self.params.pps);
            self.primed = true;
        }
        Some(BuiltUnit { keyframe: unit.keyframe, codec })
    }
}

// Public in-place variant used on pooled buffers that already hold an Annex B unit at
// `[HEADER_LEN..]`: rewrites it as AVCC and returns its keyframe flag. Length is returned too.
pub fn annexb_to_avcc_in_place(buf: &mut Vec<u8>, from: usize) -> ConvertedUnit {
    let annexb = buf.split_off(from);
    let mut params = ParamSets::default();
    convert(&annexb, buf, &mut params)
}

// ChaCha20-Poly1305 with the DataStream key. Nonce: four zero bytes then the frame counter (LE);
// AAD: the complete 128-byte header, whose size field already counts the tag.
pub struct FrameCipher {
    aead: ChaCha20Poly1305,
    counter: u64,
}

impl FrameCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: ChaCha20Poly1305::new(key.into()), counter: 0 }
    }

    pub fn seal(&mut self, header: &[u8], payload: &mut [u8]) -> Result<[u8; TAG_LEN]> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        let tag = self
            .aead
            .encrypt_inout_detached(&Nonce::from(nonce), header, payload.into())
            .map_err(|_| Error::Crypto("fallo al cifrar el fotograma".into()))?;
        self.counter += 1;
        Ok(tag.into())
    }
}

pub struct Unit {
    pub buf: Vec<u8>,
    pub capture: Instant,
    pub keyframe: bool,
    pub codec: Option<CodecConfig>,
}

#[derive(Debug, Clone, Copy)]
pub struct Sent {
    pub bytes: usize,
    pub timestamp: u64,
}

pub struct DataSender<W> {
    writer: W,
    cipher: Option<FrameCipher>,
    bias: Duration,
    last_timestamp: u64,
    codec_scratch: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> DataSender<W> {
    pub fn new(writer: W, cipher: Option<FrameCipher>, bias: Duration) -> Self {
        Self { writer, cipher, bias, last_timestamp: 0, codec_scratch: Vec::with_capacity(512) }
    }

    pub fn frame_time(&mut self, capture: Instant) -> u64 {
        let now = Instant::now();
        let base = if now.saturating_duration_since(capture) <= MAX_CAPTURE_AGE { capture } else { now };
        let timestamp = clock::video_timestamp(base, self.bias).max(self.last_timestamp + 1);
        self.last_timestamp = timestamp;
        timestamp
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        tokio::time::timeout(WRITE_TIMEOUT, self.writer.write_all(data))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(())
    }

    // The codec packet (when present) precedes the frame it belongs to; frames are sealed in place.
    pub async fn send_unit(&mut self, unit: &mut Unit) -> Result<Sent> {
        let timestamp = self.frame_time(unit.capture);
        let mut bytes = 0;
        if let Some(codec) = unit.codec.take() {
            let mut scratch = std::mem::take(&mut self.codec_scratch);
            scratch.clear();
            scratch.resize(HEADER_LEN, 0);
            codec_header(&mut scratch, codec.avcc.len(), timestamp, codec.width, codec.height);
            scratch.extend_from_slice(&codec.avcc);
            let result = self.write(&scratch).await;
            bytes += scratch.len();
            self.codec_scratch = scratch;
            result?;
        }
        let payload_len = unit.buf.len() - HEADER_LEN;
        let sealed_len = payload_len + if self.cipher.is_some() { TAG_LEN } else { 0 };
        let (header, payload) = unit.buf.split_at_mut(HEADER_LEN);
        video_header(header, sealed_len, unit.keyframe, timestamp);
        if let Some(cipher) = &mut self.cipher {
            let tag = cipher.seal(header, payload)?;
            unit.buf.extend_from_slice(&tag);
        }
        self.write(&unit.buf).await?;
        bytes += unit.buf.len();
        Ok(Sent { bytes, timestamp })
    }

    pub async fn send_heartbeat(&mut self) -> Result<()> {
        let mut header = [0u8; HEADER_LEN];
        heartbeat_header(&mut header);
        self.write(&header).await
    }
}

#[cfg(test)]
mod tests {
    use chacha20poly1305::aead::{Aead, Payload};
    use tokio::io::AsyncReadExt;

    use super::*;

    const SPS: [u8; 9] = [0x67, 0x42, 0x00, 0x1F, 0xF8, 0x0A, 0x00, 0xB7, 0x20];
    const PPS: [u8; 4] = [0x68, 0xCE, 0x06, 0xE2];

    fn float_at(raw: &[u8], at: usize) -> f32 {
        f32::from_le_bytes(raw[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn video_header_layout() {
        let mut header = [0xFFu8; HEADER_LEN];
        video_header(&mut header, 1000, true, 0x0123_4567_89AB_CDEF);
        assert_eq!(u32::from_le_bytes(header[0..4].try_into().unwrap()), 1000);
        assert_eq!((header[4], header[5], header[6], header[7]), (0, 0x10, 0, 0));
        assert_eq!(u64::from_le_bytes(header[8..16].try_into().unwrap()), 0x0123_4567_89AB_CDEF);
        assert!(header[16..].iter().all(|&b| b == 0), "the timeline field is zero for NTP");
        video_header(&mut header, 1, false, 0);
        assert_eq!(header[5], 0);
    }

    #[test]
    fn codec_header_describes_the_encoded_raster_in_every_rect() {
        let mut header = [0u8; HEADER_LEN];
        codec_header(&mut header, 3, 77, 1280, 800);
        assert_eq!((header[4], header[5], header[6], header[7]), (1, 0, 0x16, 1));
        for at in [16, 40, 56] {
            assert_eq!(float_at(&header, at), 1280.0);
            assert_eq!(float_at(&header, at + 4), 800.0);
        }
        for at in [32, 36, 48, 52] {
            assert_eq!(float_at(&header, at), 0.0);
        }
        assert_eq!(u64::from_le_bytes(header[8..16].try_into().unwrap()), 77);
    }

    #[test]
    fn heartbeat_header_layout() {
        let mut header = [0xAAu8; HEADER_LEN];
        heartbeat_header(&mut header);
        assert_eq!((header[4], header[6]), (0x02, 0x1E));
        assert_eq!(header.iter().filter(|&&b| b != 0).count(), 2);
    }

    #[test]
    fn avcc_is_built_from_sps_and_pps() {
        let avcc = build_avcc(&SPS, &PPS);
        assert_eq!(&avcc[..6], &[1, 0x42, 0x00, 0x1F, 0xFF, 0xE1]);
        assert_eq!(u16::from_be_bytes([avcc[6], avcc[7]]) as usize, SPS.len());
        assert_eq!(&avcc[8..8 + SPS.len()], &SPS);
        let at = 8 + SPS.len();
        assert_eq!(avcc[at], 1);
        assert_eq!(u16::from_be_bytes([avcc[at + 1], avcc[at + 2]]) as usize, PPS.len());
        assert_eq!(&avcc[at + 3..at + 3 + PPS.len()], &PPS);
        assert_eq!(&avcc[avcc.len() - 4..], &[2, 0, 0, 0]);
        assert_eq!(avcc.len(), at + 3 + PPS.len() + 4);
    }

    #[test]
    fn sps_dimensions_match_doubletake_vectors() {
        assert_eq!(sps_dimensions(&SPS), Some((1280, 720)));
        assert_eq!(sps_dimensions(&[0x61, 0x80, 0, 0]), None);
        assert_eq!(sps_dimensions(&[0x67, 0x42]), None);
    }

    #[test]
    fn start_codes_of_both_lengths_are_split() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
        let nals: Vec<&[u8]> = split_annexb(&stream).collect();
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert_eq!(split_annexb(&[1, 2, 3]).count(), 0);
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn annexb_becomes_avcc_without_parameter_sets_aud_or_sei() {
        let idr = [0x65u8, 0x88, 0x84, 0x00, 0x10];
        let stream = annexb(&[&[0x09, 0xF0], &SPS, &PPS, &[0x06, 0x05, 0x01], &idr]);
        let mut out = vec![0u8; 10];
        let mut params = ParamSets::default();
        let unit = convert(&stream, &mut out, &mut params);
        assert_eq!(unit, ConvertedUnit { keyframe: true, vcl_nals: 1 });
        let mut expected = vec![0u8; 10];
        expected.extend_from_slice(&(idr.len() as u32).to_be_bytes());
        expected.extend_from_slice(&idr);
        assert_eq!(out, expected);
        assert_eq!(params.sps, SPS);
        assert_eq!(params.pps, PPS);
    }

    #[test]
    fn in_place_conversion_keeps_the_header_room() {
        let p = [0x41u8, 0x9A, 0x01];
        let mut buf = vec![0u8; HEADER_LEN];
        buf.extend_from_slice(&annexb(&[&p, &p]));
        let unit = annexb_to_avcc_in_place(&mut buf, HEADER_LEN);
        assert_eq!(unit, ConvertedUnit { keyframe: false, vcl_nals: 2 });
        assert_eq!(buf.len(), HEADER_LEN + 2 * (4 + p.len()));
        assert_eq!(&buf[HEADER_LEN..HEADER_LEN + 4], &(p.len() as u32).to_be_bytes());
        assert_eq!(&buf[HEADER_LEN + 4..HEADER_LEN + 7], &p);
    }

    #[test]
    fn builder_primes_on_the_first_idr_and_repeats_config_only_on_change() {
        let mut builder = UnitBuilder::new((640, 360));
        let mut out = Vec::new();
        let p_frame = annexb(&[&[0x41, 0x9A, 0x01]]);
        assert!(builder.build(&p_frame, &mut out).is_none(), "no picture before the first IDR");
        let idr = annexb(&[&SPS, &PPS, &[0x65, 0x88, 0x84]]);
        let first = builder.build(&idr, &mut out).unwrap();
        assert!(first.keyframe);
        let codec = first.codec.unwrap();
        assert_eq!((codec.width, codec.height), (1280, 720));
        assert_eq!(codec.avcc, build_avcc(&SPS, &PPS));
        let second = builder.build(&idr, &mut out).unwrap();
        assert!(second.codec.is_none(), "an identical configuration is not repeated");
        assert!(builder.build(&p_frame, &mut out).unwrap().codec.is_none());
        let mut pps = PPS;
        pps[3] ^= 1;
        let changed = annexb(&[&SPS, &pps, &[0x65, 0x88, 0x84]]);
        let third = builder.build(&changed, &mut out).unwrap();
        assert_eq!(third.codec.unwrap().avcc, build_avcc(&SPS, &pps));
        assert_eq!(out.len(), HEADER_LEN + 4 + 3);
    }

    #[test]
    fn idr_without_inline_parameter_sets_is_dropped_until_known() {
        let mut builder = UnitBuilder::new((640, 360));
        let mut out = Vec::new();
        let idr = annexb(&[&[0x65, 0x88, 0x84]]);
        assert!(builder.build(&idr, &mut out).is_none());
        builder.seed(&SPS, &PPS);
        assert!(builder.build(&idr, &mut out).unwrap().codec.is_some());
    }

    #[test]
    fn frame_cipher_round_trips_with_the_header_as_aad() {
        let key = [7u8; 32];
        let mut cipher = FrameCipher::new(&key);
        let opener = ChaCha20Poly1305::new((&key).into());
        for counter in 0u64..3 {
            let plain: Vec<u8> = (0..200u32).map(|i| (i * 7 + counter as u32) as u8).collect();
            let mut header = [0u8; HEADER_LEN];
            video_header(&mut header, plain.len() + TAG_LEN, counter == 0, 100 + counter);
            let mut payload = plain.clone();
            let tag = cipher.seal(&header, &mut payload).unwrap();
            assert_ne!(payload, plain);
            payload.extend_from_slice(&tag);
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&counter.to_le_bytes());
            let opened = opener
                .decrypt(&Nonce::from(nonce), Payload { msg: &payload, aad: &header })
                .unwrap();
            assert_eq!(opened, plain);
            let mut tampered = header;
            tampered[5] ^= 0x10;
            assert!(opener.decrypt(&Nonce::from(nonce), Payload { msg: &payload, aad: &tampered }).is_err());
        }
    }

    #[test]
    fn frame_cipher_uses_the_audio_sealer_keystream() {
        let key = [9u8; 32];
        let header = [3u8; HEADER_LEN];
        let mut payload = *b"payload bytes!!";
        FrameCipher::new(&key).seal(&header, &mut payload).unwrap();
        let mut reference = b"payload bytes!!".to_vec();
        let length = reference.len();
        let mut audio_header = [0u8; 12];
        audio_header[4..].copy_from_slice(&header[4..12]);
        rustplay_core::crypto::stream::AudioSealer::new(key)
            .seal_in_place(&audio_header, &mut reference, 0)
            .unwrap();
        assert_eq!(&reference[..length], &payload[..]);
    }

    #[tokio::test]
    async fn sender_writes_codec_then_sealed_frame_and_heartbeat() {
        let (writer, mut reader) = tokio::io::duplex(1 << 16);
        let key = [4u8; 32];
        let mut sender = DataSender::new(writer, Some(FrameCipher::new(&key)), Duration::from_millis(75));
        let plain = [0, 0, 0, 3, 0x65, 1, 2];
        let mut buf = vec![0u8; HEADER_LEN];
        buf.extend_from_slice(&plain);
        let mut unit = Unit {
            buf,
            capture: Instant::now(),
            keyframe: true,
            codec: Some(CodecConfig { avcc: vec![9; 20], width: 640, height: 360 }),
        };
        let sent = sender.send_unit(&mut unit).await.unwrap();
        sender.send_heartbeat().await.unwrap();
        assert_eq!(sent.bytes, HEADER_LEN + 20 + HEADER_LEN + plain.len() + TAG_LEN);

        let mut codec = vec![0u8; HEADER_LEN + 20];
        reader.read_exact(&mut codec).await.unwrap();
        assert_eq!(codec[4], 1);
        assert_eq!(u32::from_le_bytes(codec[0..4].try_into().unwrap()), 20);
        assert_eq!(&codec[HEADER_LEN..], &[9u8; 20]);

        let mut frame = vec![0u8; HEADER_LEN + plain.len() + TAG_LEN];
        reader.read_exact(&mut frame).await.unwrap();
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize, plain.len() + TAG_LEN);
        assert_eq!((frame[4], frame[5]), (0, 0x10));
        assert_eq!(&frame[8..16], &codec[8..16], "codec packet and frame share the timestamp");
        let opener = ChaCha20Poly1305::new((&key).into());
        let opened = opener
            .decrypt(&Nonce::from([0u8; 12]), Payload { msg: &frame[HEADER_LEN..], aad: &frame[..HEADER_LEN] })
            .unwrap();
        assert_eq!(opened, plain);

        let mut heartbeat = vec![0u8; HEADER_LEN];
        reader.read_exact(&mut heartbeat).await.unwrap();
        assert_eq!((heartbeat[4], heartbeat[6]), (2, 0x1E));
    }

    #[tokio::test]
    async fn sender_without_cipher_sends_the_payload_as_is_and_timestamps_grow() {
        let (writer, mut reader) = tokio::io::duplex(1 << 16);
        let mut sender = DataSender::new(writer, None, Duration::from_millis(75));
        let capture = Instant::now();
        let mut stamps = Vec::new();
        for _ in 0..2 {
            let mut buf = vec![0u8; HEADER_LEN];
            buf.extend_from_slice(&[1, 2, 3, 4]);
            let mut unit = Unit { buf, capture, keyframe: false, codec: None };
            stamps.push(sender.send_unit(&mut unit).await.unwrap().timestamp);
        }
        assert!(stamps[1] > stamps[0], "the same capture time still yields increasing timestamps");
        let mut raw = vec![0u8; 2 * (HEADER_LEN + 4)];
        reader.read_exact(&mut raw).await.unwrap();
        assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), 4);
        assert_eq!(&raw[HEADER_LEN..HEADER_LEN + 4], &[1, 2, 3, 4]);
    }
}
