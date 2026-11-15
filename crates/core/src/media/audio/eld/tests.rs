// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

use super::bits::BitReader;
use super::channel::decode_band;
use super::config::{ASC_APPLE_ELD_44100, AudioSpecificConfig, FRAME_LENGTH};
use super::filterbank::{EldFilterbank, Imdct, N};
use super::huffman::Codebooks;
use super::tables::*;
use super::{EldDecoder, create};
use crate::media::audio::{AudioCodec, AudioFormat, Decoder, create_decoder};
use std::f64::consts::PI;

struct BitWriter {
    bytes: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            nbits: 0,
        }
    }

    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            if self.nbits % 8 == 0 {
                self.bytes.push(0);
            }
            if (v >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.nbits % 8);
            }
            self.nbits += 1;
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

fn spectral_code(cb: usize, sym: usize) -> (u32, u32) {
    macro_rules! pick {
        ($c:expr, $b:expr) => {
            (u32::from($c[sym]), u32::from($b[sym]))
        };
    }
    match cb {
        1 => pick!(SPECTRAL_CODES_1, SPECTRAL_BITS_1),
        2 => pick!(SPECTRAL_CODES_2, SPECTRAL_BITS_2),
        3 => pick!(SPECTRAL_CODES_3, SPECTRAL_BITS_3),
        4 => pick!(SPECTRAL_CODES_4, SPECTRAL_BITS_4),
        5 => pick!(SPECTRAL_CODES_5, SPECTRAL_BITS_5),
        6 => pick!(SPECTRAL_CODES_6, SPECTRAL_BITS_6),
        7 => pick!(SPECTRAL_CODES_7, SPECTRAL_BITS_7),
        8 => pick!(SPECTRAL_CODES_8, SPECTRAL_BITS_8),
        9 => pick!(SPECTRAL_CODES_9, SPECTRAL_BITS_9),
        10 => pick!(SPECTRAL_CODES_10, SPECTRAL_BITS_10),
        _ => pick!(SPECTRAL_CODES_11, SPECTRAL_BITS_11),
    }
}

fn put_spectral(bw: &mut BitWriter, cb: usize, sym: usize) {
    let (c, n) = spectral_code(cb, sym);
    bw.put(c, n);
}

fn put_escape(bw: &mut BitWriter, n: u32) {
    let b = 31 - n.leading_zeros();
    for _ in 0..b - 4 {
        bw.put(1, 1);
    }
    bw.put(0, 1);
    bw.put(n - (1 << b), b);
}

// Encodes integer quantized values for one band with the given codebook.
fn encode_band(bw: &mut BitWriter, cb: usize, values: &[i32]) {
    match cb {
        1 | 2 => {
            for q in values.chunks(4) {
                let sym = q.iter().fold(0usize, |a, &v| a * 3 + (v + 1) as usize);
                put_spectral(bw, cb, sym);
            }
        }
        3 | 4 => {
            for q in values.chunks(4) {
                let sym = q
                    .iter()
                    .fold(0usize, |a, &v| a * 3 + v.unsigned_abs() as usize);
                put_spectral(bw, cb, sym);
                for &v in q.iter().filter(|&&v| v != 0) {
                    bw.put(u32::from(v < 0), 1);
                }
            }
        }
        5 | 6 => {
            for q in values.chunks(2) {
                let sym = ((q[0] + 4) * 9 + q[1] + 4) as usize;
                put_spectral(bw, cb, sym);
            }
        }
        7..=10 => {
            let dim = if cb < 9 { 8 } else { 13 };
            for q in values.chunks(2) {
                let sym = q[0].unsigned_abs() as usize * dim + q[1].unsigned_abs() as usize;
                put_spectral(bw, cb, sym);
                for &v in q.iter().filter(|&&v| v != 0) {
                    bw.put(u32::from(v < 0), 1);
                }
            }
        }
        _ => {
            for q in values.chunks(2) {
                let m = |v: i32| (v.unsigned_abs() as usize).min(16);
                put_spectral(bw, 11, m(q[0]) * 17 + m(q[1]));
                for &v in q.iter().filter(|&&v| v != 0) {
                    bw.put(u32::from(v < 0), 1);
                }
                for &v in q {
                    if v.unsigned_abs() >= 16 {
                        put_escape(bw, v.unsigned_abs());
                    }
                }
            }
        }
    }
}

fn expected(v: i32, sf: f32) -> f32 {
    let m = f64::from(v.unsigned_abs()).powf(4.0 / 3.0) as f32 * sf;
    if v < 0 { -m } else { m }
}

#[test]
fn bit_reader_reads_and_flags_overrun() {
    let data = [0b1010_1100, 0xFF, 0x01];
    let mut br = BitReader::new(&data);
    assert_eq!(br.read(3), 0b101);
    assert_eq!(br.read(5), 0b01100);
    assert_eq!(br.read(8), 0xFF);
    assert_eq!(br.read(7), 0);
    assert!(!br.overrun());
    assert_eq!(br.read_bit(), 1);
    assert_eq!(br.bits_left(), 0);
    assert!(!br.overrun());
    assert_eq!(br.read(32), 0);
    assert!(br.overrun());
    let mut br = BitReader::new(&[0xDE, 0xAD, 0xBE, 0xEF, 0x12]);
    br.skip(4);
    assert_eq!(br.read(32), 0xEADB_EEF1);
    assert_eq!(br.position(), 36);
}

#[test]
fn asc_parses_apple_eld() {
    let c = AudioSpecificConfig::parse(&ASC_APPLE_ELD_44100).unwrap();
    assert_eq!(c.object_type, 39);
    assert_eq!(c.sample_rate, 44100);
    assert_eq!(c.channel_config, 2);
    assert_eq!(c.frame_length, 480);
    assert!(!c.sbr);
    let s = c.swb_info().unwrap();
    assert_eq!((s.num_swb, s.tns_max_bands), (35, 32));
    assert_eq!(usize::from(s.offsets[s.num_swb]), 480);
}

#[test]
fn asc_rejects_sbr_and_other_types() {
    let err = AudioSpecificConfig::parse(&[0xF8, 0xE8, 0x51, 0x00]).unwrap_err();
    assert!(err.to_string().contains("AAC-ELD con SBR no soportado"));
    assert!(AudioSpecificConfig::parse(&[0x12, 0x10]).is_err());
    assert!(AudioSpecificConfig::parse(&[0xF8]).is_err());
    assert!(AudioSpecificConfig::parse(&[]).is_err());
}

#[test]
fn every_codeword_roundtrips_through_the_tree() {
    let cb = Codebooks::new();
    let sizes = [81, 81, 81, 81, 81, 81, 64, 64, 169, 169, 289];
    for (i, &size) in sizes.iter().enumerate() {
        for sym in 0..size {
            let mut bw = BitWriter::new();
            put_spectral(&mut bw, i + 1, sym);
            bw.put(0, 24);
            let bytes = bw.finish();
            let mut br = BitReader::new(&bytes);
            assert_eq!(cb.spectral[i].decode(&mut br), Some(sym), "cb {}", i + 1);
        }
    }
    for sym in 0..121 {
        let mut bw = BitWriter::new();
        bw.put(SCALEFACTOR_CODE[sym], u32::from(SCALEFACTOR_BITS[sym]));
        bw.put(0, 24);
        let bytes = bw.finish();
        let mut br = BitReader::new(&bytes);
        assert_eq!(cb.scalefactor.decode(&mut br), Some(sym));
    }
}

#[test]
fn spectral_codebooks_decode_handcrafted_values() {
    let cb = Codebooks::new();
    let sf = 0.5f32;
    let cases: Vec<(usize, Vec<i32>)> = vec![
        (1, vec![1, 0, -1, 1, -1, -1, 0, 0]),
        (2, vec![0, 1, 1, -1, 1, 1, 1, -1]),
        (3, vec![2, 0, 1, -1, -2, 1, 0, 2]),
        (4, vec![0, 0, 2, -2, 1, 1, -1, 0]),
        (5, vec![4, -4, 0, 3, -1, 2, 1, -3]),
        (6, vec![-4, 4, 2, 0, 1, -1, 3, -2]),
        (7, vec![7, -7, 0, 3, 1, 0, -5, 6]),
        (8, vec![0, 7, -1, -2, 4, 5, 0, 0]),
        (9, vec![12, -12, 0, 5, 7, 0, -1, 9]),
        (10, vec![0, 12, -3, 8, 6, -6, 11, 1]),
        (11, vec![0, 15, -16, 17, 100, -4000, 33, 0, 8191, -9]),
    ];
    for (book, values) in cases {
        let mut bw = BitWriter::new();
        encode_band(&mut bw, book, &values);
        bw.put(0, 32);
        let bytes = bw.finish();
        let mut br = BitReader::new(&bytes);
        let mut out = vec![0.0f32; values.len()];
        decode_band(&mut br, &cb, book as u8, sf, &mut out).unwrap();
        for (o, &v) in out.iter().zip(&values) {
            let e = expected(v, sf);
            assert!((o - e).abs() <= e.abs() * 1e-6, "cb {book}: {o} vs {e}");
        }
        assert!(!br.overrun());
    }
}

#[test]
fn escape_overflow_is_an_error() {
    let cb = Codebooks::new();
    let mut bw = BitWriter::new();
    put_spectral(&mut bw, 11, 16 * 17);
    bw.put(0, 1);
    for _ in 0..12 {
        bw.put(1, 1);
    }
    bw.put(0, 32);
    bw.put(0, 8);
    let bytes = bw.finish();
    let mut br = BitReader::new(&bytes);
    let mut out = [0.0f32; 2];
    assert!(decode_band(&mut br, &cb, 11, 1.0, &mut out).is_err());
}

fn reference_half_imdct(x: &[f32; N]) -> Vec<f32> {
    (0..N)
        .map(|j| {
            let s: f64 = (0..N)
                .map(|k| {
                    f64::from(x[k])
                        * (PI / N as f64 * (j as f64 + N as f64 + 0.5) * (k as f64 + 0.5)).cos()
                })
                .sum();
            (-s / N as f64) as f32
        })
        .collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

#[test]
fn fast_imdct_matches_direct_formula() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut imdct = Imdct::new();
    for _ in 0..4 {
        let mut x = [0.0f32; N];
        for v in &mut x {
            *v = rng.unit();
        }
        let mut out = [0.0f32; N];
        imdct.process(&x, &mut out);
        let r = reference_half_imdct(&x);
        let err = out
            .iter()
            .zip(&r)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-4, "max error {err}");
    }
}

// Windowing written per frame: frame 0 is the current IMDCT output and
// frames 1..3 the previous ones.
fn reference_eld_window(h: [&[f32]; 4]) -> Vec<f32> {
    let w = &ELD_WINDOW_480;
    let (n, n2, n4) = (N, N / 2, N / 4);
    let mut out = vec![0.0f32; N];
    for m in 0..n4 {
        let i = m + n4;
        out[m] = h[0][n2 - 1 - i] * w[m] + h[1][i + n2] * w[m + n]
            - h[2][n2 - 1 - i] * w[m + 2 * n]
            - h[3][n2 + i] * w[m + 3 * n];
    }
    for i in 0..n2 {
        out[n4 + i] = h[0][i] * w[i + n2 - n4]
            - h[1][n - 1 - i] * w[i + n2 + n - n4]
            - h[2][i] * w[i + n2 + 2 * n - n4]
            + h[3][n - 1 - i] * w[i + n2 + 3 * n - n4];
    }
    for i in 0..n4 {
        out[n2 + n4 + i] = h[0][i + n2] * w[i + n - n4]
            - h[1][n2 - 1 - i] * w[i + 2 * n - n4]
            - h[2][n2 + i] * w[i + 3 * n - n4];
    }
    out
}

#[test]
fn eld_filterbank_matches_direct_implementation() {
    let mut rng = Rng(0x0dd_ba11_cafe_f00d);
    let mut fb = EldFilterbank::new();
    let mut history: Vec<Vec<f32>> = vec![vec![0.0; N]; 3];
    for frame in 0..8 {
        let mut coeffs = [0.0f32; N];
        for v in &mut coeffs {
            *v = rng.unit() * 100.0;
        }
        let orig = coeffs;
        let mut out = [0.0f32; N];
        fb.process(&mut coeffs, &mut out);

        let mut perm = orig;
        for i in (0..N / 2).step_by(2) {
            let t = perm[i];
            perm[i] = -perm[N - 1 - i];
            perm[N - 1 - i] = t;
            let t = -perm[i + 1];
            perm[i + 1] = perm[N - 2 - i];
            perm[N - 2 - i] = t;
        }
        let mut cur = reference_half_imdct(&perm);
        for i in (0..N).step_by(2) {
            cur[i] = -cur[i];
        }
        let reference = reference_eld_window([&cur, &history[0], &history[1], &history[2]]);
        let err = out
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "frame {frame}: max error {err}");
        history.insert(0, cur);
        history.truncate(3);
    }
}

fn put_sections(bw: &mut BitWriter, band: usize, cb: u32) {
    if band > 0 {
        bw.put(0, 4);
        bw.put(band as u32, 5);
    }
    bw.put(cb, 4);
    bw.put(1, 5);
}

fn tone_frame(band: usize, cb: usize, gain: u32, values: &[i32]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.put(band as u32 + 1, 6);
    bw.put(0, 2);
    for _ in 0..2 {
        bw.put(gain, 8);
        put_sections(&mut bw, band, cb as u32);
        bw.put(SCALEFACTOR_CODE[60], u32::from(SCALEFACTOR_BITS[60]));
        bw.put(0, 1);
        encode_band(&mut bw, cb, values);
    }
    bw.finish()
}

fn silent_frame() -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.put(0, 6);
    bw.put(0, 2);
    for _ in 0..2 {
        bw.put(0, 8);
        bw.put(0, 1);
    }
    bw.finish()
}

fn goertzel(x: &[f32], freq: f64, rate: f64) -> f64 {
    let w = 2.0 * PI * freq / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = f64::from(v) + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2) / (x.len() as f64)
}

// Band 8 spans lines 32..36; the 12 sits on line 34.
const TONE_LINE: usize = 34;

#[test]
fn synthetic_stream_decodes_tone_and_silence() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let silent = silent_frame();
    let tone = tone_frame(8, 9, 160, &[0, 0, 12, 0]);
    let mut sequence: Vec<&Vec<u8>> = vec![&silent, &silent];
    sequence.extend([&tone; 4]);
    sequence.extend([&silent; 5]);
    let mut left: Vec<Vec<f32>> = Vec::new();
    for (i, pkt) in sequence.iter().enumerate() {
        let n = dec.decode(pkt, &mut out).unwrap();
        assert_eq!(n, FRAME_LENGTH);
        let l: Vec<f32> = out.iter().step_by(2).map(|&v| f32::from(v)).collect();
        let r: Vec<f32> = out
            .iter()
            .skip(1)
            .step_by(2)
            .map(|&v| f32::from(v))
            .collect();
        assert_eq!(l, r);
        assert!(l.iter().all(|v| v.is_finite()));
        if i < 2 {
            assert!(out.iter().all(|&v| v == 0), "frame {i} not silent");
        }
        left.push(l);
    }
    assert!(left.last().unwrap().iter().all(|&v| v == 0.0));
    let tone_pcm: Vec<f32> = left[2..10].concat();
    let peak = tone_pcm.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
    assert!(peak > 100.0 && peak < 32767.0, "peak {peak}");
    let f0 = (TONE_LINE as f64 + 0.5) * 22050.0 / 480.0;
    let p_in = (-60..=60)
        .map(|d| goertzel(&tone_pcm, f0 + f64::from(d) * 5.0, 44100.0))
        .fold(0.0, f64::max);
    for f in [300.0, 800.0, 3000.0, 6000.0, 12000.0] {
        let p = goertzel(&tone_pcm, f, 44100.0);
        assert!(p_in > 1000.0 * p, "tone {p_in} vs {f} Hz {p}");
    }
}

#[test]
fn m_s_and_noise_bands_decode_without_error() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut bw = BitWriter::new();
    bw.put(2, 6);
    bw.put(2, 2);
    for _ in 0..2 {
        bw.put(120, 8);
        bw.put(13, 4);
        bw.put(1, 5);
        bw.put(9, 4);
        bw.put(1, 5);
        bw.put(300 - 256 + 256, 9);
        bw.put(SCALEFACTOR_CODE[60], u32::from(SCALEFACTOR_BITS[60]));
        bw.put(0, 1);
        encode_band(&mut bw, 9, &[1, 2, 3, 4]);
    }
    let mut out = [0i16; FRAME_LENGTH * 2];
    assert_eq!(dec.decode(&bw.finish(), &mut out).unwrap(), FRAME_LENGTH);
    assert!(out.iter().any(|&v| v != 0));
}

#[test]
fn tns_filter_decodes_and_applies() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut bw = BitWriter::new();
    bw.put(9, 6);
    bw.put(0, 2);
    for _ in 0..2 {
        bw.put(140, 8);
        put_sections(&mut bw, 8, 9);
        bw.put(SCALEFACTOR_CODE[60], u32::from(SCALEFACTOR_BITS[60]));
        bw.put(1, 1);
        bw.put(1, 2);
        bw.put(1, 1);
        bw.put(20, 6);
        bw.put(4, 5);
        bw.put(0, 1);
        bw.put(0, 1);
        for c in [3, 9, 12, 6] {
            bw.put(c, 4);
        }
        encode_band(&mut bw, 9, &[0, 0, 12, 0]);
    }
    let mut out = [0i16; FRAME_LENGTH * 2];
    dec.decode(&bw.finish(), &mut out).unwrap();
    assert!(out.iter().any(|&v| v != 0));
}

#[test]
fn truncated_and_invalid_frames_are_errors() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let frame = tone_frame(8, 9, 160, &[0, 0, 12, 0]);
    assert!(dec.decode(&frame[..frame.len() - 3], &mut out).is_err());
    assert!(dec.decode(&[], &mut out).is_err());
    assert!(dec.decode(&[0xFF; 4], &mut out).is_err());
    assert!(dec.decode(&frame, &mut out[..10]).is_err());
    assert!(dec.decode(&frame, &mut out).is_ok());
}

#[test]
fn random_input_never_panics() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let mut rng = Rng(0xfeed_face_dead_beef);
    let (mut ok, mut err) = (0, 0);
    for i in 0..10_000 {
        let len = (rng.next() % 220) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        if i % 3 == 0 {
            for b in data.iter_mut().skip(2) {
                *b &= (rng.next() as u8) | (rng.next() as u8);
            }
        }
        match dec.decode(&data, &mut out) {
            Ok(_) => ok += 1,
            Err(_) => err += 1,
        }
    }
    assert!(ok + err == 10_000);
}

#[test]
fn mutated_valid_frames_never_panic() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let mut rng = Rng(0x5eed_5eed_5eed_5eed);
    let base = tone_frame(8, 9, 160, &[0, 0, 12, 0]);
    for _ in 0..10_000 {
        let mut f = base.clone();
        for _ in 0..(rng.next() % 4 + 1) {
            let pos = (rng.next() as usize) % f.len();
            f[pos] ^= 1 << (rng.next() % 8);
        }
        let _ = dec.decode(&f, &mut out);
    }
}

#[test]
fn create_decoder_builds_eld() {
    let fmt = AudioFormat {
        codec: AudioCodec::AacEld,
        sample_rate: 44100,
        channels: 2,
        frames_per_packet: 480,
    };
    let mut d = create_decoder(&fmt).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    assert_eq!(d.decode(&silent_frame(), &mut out).unwrap(), FRAME_LENGTH);
    assert!(create(&fmt).is_ok());
}

#[test]
#[ignore]
fn perf_decode_packet() {
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let mut bw = BitWriter::new();
    bw.put(35, 6);
    bw.put(0, 2);
    let mut rng = Rng(7);
    for _ in 0..2 {
        bw.put(150, 8);
        bw.put(9, 4);
        bw.put(31, 5);
        bw.put(4, 5);
        for _ in 0..35 {
            bw.put(SCALEFACTOR_CODE[60], u32::from(SCALEFACTOR_BITS[60]));
        }
        bw.put(0, 1);
        let offs = &SWB_OFFSET_480_48;
        for b in 0..35 {
            let width = usize::from(offs[b + 1] - offs[b]);
            let vals: Vec<i32> = (0..width).map(|_| (rng.next() % 9) as i32 - 4).collect();
            let vals: Vec<i32> = vals.iter().map(|v| v.abs().min(2)).collect();
            encode_band(&mut bw, 9, &vals);
        }
    }
    let frame = bw.finish();
    dec.decode(&frame, &mut out).unwrap();
    let iterations = 2000;
    let t = std::time::Instant::now();
    for _ in 0..iterations {
        dec.decode(&frame, &mut out).unwrap();
    }
    let per = t.elapsed().as_secs_f64() * 1e6 / f64::from(iterations);
    println!(
        "ELD decode: {per:.1} us per 480-frame packet ({} bytes)",
        frame.len()
    );
}

#[test]
#[ignore]
fn capture_decodes_without_errors() {
    let Ok(path) = std::env::var("RUSTPLAY_ELD_CAPTURE") else {
        eprintln!("RUSTPLAY_ELD_CAPTURE not set");
        return;
    };
    let data = std::fs::read(path).unwrap();
    assert!(data.len() >= 13, "capture too short");
    assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), 0);
    let ct = data[4];
    let spf = u32::from_le_bytes(data[5..9].try_into().unwrap());
    let sr = u32::from_le_bytes(data[9..13].try_into().unwrap());
    println!("capture ct={ct} spf={spf} sr={sr}");
    let mut dec = EldDecoder::new(&ASC_APPLE_ELD_44100).unwrap();
    let mut out = [0i16; FRAME_LENGTH * 2];
    let mut pcm: Vec<u8> = Vec::new();
    let (mut pos, mut packets, mut errors) = (13usize, 0usize, 0usize);
    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let Some(payload) = data.get(pos..pos + len) else {
            break;
        };
        pos += len;
        packets += 1;
        match dec.decode(payload, &mut out) {
            Ok(n) => {
                for s in &out[..n * 2] {
                    pcm.extend_from_slice(&s.to_le_bytes());
                }
            }
            Err(e) => {
                errors += 1;
                if errors <= 5 {
                    println!("packet {packets} ({len} bytes): {e}");
                }
            }
        }
    }
    println!("{packets} packets, {errors} errors");
    if let Ok(p) = std::env::var("RUSTPLAY_ELD_PCM_OUT") {
        std::fs::write(p, &pcm).unwrap();
    }
    assert_eq!(errors, 0);
}
