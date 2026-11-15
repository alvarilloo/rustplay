// Ported from doubletake alac_compressed.go (LGPL-3.0-or-later)

use crate::{Error, Result};

const CHANNELS: usize = 2;
const SAMPLE_BITS: u32 = 16;
const CODED_BITS: u32 = SAMPLE_BITS + 1;

const PREDICTION_SHIFT: u32 = 4;
const PREDICTION_HALF: i32 = 1 << (PREDICTION_SHIFT - 1);
const PREDICTION_SCALE: i32 = 1 << PREDICTION_SHIFT;
const MIN_ORDER: usize = 1;
const MAX_ORDER: usize = 4;

const STEREO_SHIFT: u32 = 2;

const HISTORY_START: u32 = 10;
const HISTORY_MULTIPLIER: u32 = 40;
const HISTORY_FRACTION_BITS: u32 = 9;
const HISTORY_ZERO_THRESHOLD: u32 = 1 << HISTORY_FRACTION_BITS;
const HISTORY_CLAMP: u32 = 0xffff;
const RICE_LIMIT: u32 = 14;
const ESCAPE_PREFIX: u32 = 9;

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        out.clear();
        Self {
            out,
            acc: 0,
            nbits: 0,
        }
    }

    fn write(&mut self, value: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        let mask = if bits >= 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << bits) - 1
        };
        self.acc = (self.acc << bits) | (u64::from(value) & mask);
        self.nbits += bits;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1u64 << self.nbits) - 1;
    }

    fn finish(mut self) {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.write(0, pad);
        }
    }
}

pub struct AlacEncoder {
    pub frames_per_packet: usize,
    pcm: [Vec<i32>; CHANNELS],
    coded: [Vec<i32>; CHANNELS],
    residual: [Vec<i32>; CHANNELS],
    candidate: Vec<i32>,
    attempt: Vec<u8>,
    best: Vec<u8>,
}

impl AlacEncoder {
    pub fn new(frames_per_packet: usize) -> Self {
        let n = frames_per_packet;
        let samples = || vec![0i32; n];
        let capacity = n * CHANNELS * 2 + 64;
        Self {
            frames_per_packet,
            pcm: [samples(), samples()],
            coded: [samples(), samples()],
            residual: [samples(), samples()],
            candidate: samples(),
            attempt: Vec::with_capacity(capacity),
            best: Vec::with_capacity(capacity),
        }
    }

    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        let n = self.frames_per_packet;
        if n == 0 || pcm.len() != n * CHANNELS {
            return Err(Error::Media(format!(
                "ALAC: se esperaban {} muestras intercaladas y llegaron {}",
                n * CHANNELS,
                pcm.len()
            )));
        }
        for (i, frame) in pcm.chunks_exact(CHANNELS).enumerate() {
            self.pcm[0][i] = i32::from(frame[0]);
            self.pcm[1][i] = i32::from(frame[1]);
        }

        let mut best = std::mem::take(&mut self.best);
        let mut attempt = std::mem::take(&mut self.attempt);
        self.encode_stereo_mode(&mut best, 0);
        for weight in 1..=(1i32 << STEREO_SHIFT) {
            self.encode_stereo_mode(&mut attempt, weight);
            if attempt.len() < best.len() {
                std::mem::swap(&mut best, &mut attempt);
            }
        }

        let verbatim_bytes =
            (3 + 4 + 12 + 4 + 32 + n * CHANNELS * SAMPLE_BITS as usize + 3).div_ceil(8);
        if best.len() >= verbatim_bytes {
            encode_verbatim(out, pcm, n);
        } else {
            out.clear();
            out.extend_from_slice(&best);
        }
        self.best = best;
        self.attempt = attempt;
        Ok(())
    }

    fn encode_stereo_mode(&mut self, out: &mut Vec<u8>, weight: i32) {
        let n = self.frames_per_packet;
        for i in 0..n {
            let left = self.pcm[0][i];
            let right = self.pcm[1][i];
            if weight == 0 {
                self.coded[0][i] = left;
                self.coded[1][i] = right;
            } else {
                let diff = left - right;
                self.coded[0][i] = right + ((diff * weight) >> STEREO_SHIFT);
                self.coded[1][i] = diff;
            }
        }

        let mut orders = [0usize; CHANNELS];
        for (ch, order) in orders.iter_mut().enumerate() {
            *order = select_predictor(&self.coded[ch], &mut self.residual[ch], &mut self.candidate);
        }

        let mut w = BitWriter::new(out);
        w.write(1, 3);
        w.write(0, 4);
        w.write(0, 12);
        w.write(8, 4);
        w.write(n as u32, 32);
        w.write(STEREO_SHIFT, 8);
        w.write(weight as u32 & 0xff, 8);
        write_predictor_header(&mut w, orders[0]);
        write_predictor_header(&mut w, orders[1]);
        write_entropy_block(&mut w, &self.residual[0]);
        write_entropy_block(&mut w, &self.residual[1]);
        w.write(7, 3);
        w.finish();
    }
}

fn encode_verbatim(out: &mut Vec<u8>, pcm: &[i16], frames: usize) {
    let mut w = BitWriter::new(out);
    w.write(1, 3);
    w.write(0, 4);
    w.write(0, 12);
    w.write(1, 1);
    w.write(0, 2);
    w.write(1, 1);
    w.write(frames as u32, 32);
    for &s in pcm {
        w.write(u32::from(s as u16), SAMPLE_BITS);
    }
    w.write(7, 3);
    w.finish();
}

fn select_predictor(samples: &[i32], residuals: &mut [i32], candidate: &mut [i32]) -> usize {
    let mut best_order = MIN_ORDER;
    let mut best_bits = usize::MAX;
    for order in MIN_ORDER..=MAX_ORDER {
        predict_residuals(samples, candidate, order);
        let bits = entropy_bit_count(candidate, CODED_BITS) + 16 + order * 16;
        if bits < best_bits {
            best_bits = bits;
            best_order = order;
            residuals.copy_from_slice(candidate);
        }
    }
    best_order
}

fn prediction_coefficients(order: usize) -> [i16; MAX_ORDER] {
    let mut coefficients = [0i16; MAX_ORDER];
    let mut combination = 1i32;
    for (tap, slot) in coefficients.iter_mut().enumerate().take(order) {
        combination = combination * (order - tap) as i32 / (tap + 1) as i32;
        let mut c = combination * PREDICTION_SCALE;
        if tap & 1 != 0 {
            c = -c;
        }
        *slot = c as i16;
    }
    coefficients
}

fn narrow_signed(value: i32, width: u32) -> i32 {
    let shift = 32 - width;
    (value << shift) >> shift
}

fn predict_residuals(samples: &[i32], residuals: &mut [i32], order: usize) {
    if samples.is_empty() {
        return;
    }
    residuals[0] = samples[0];
    let warmup = order.min(samples.len() - 1);
    for i in 1..=warmup {
        residuals[i] = narrow_signed(samples[i] - samples[i - 1], CODED_BITS);
    }
    let mut coefficients = prediction_coefficients(order);
    for i in (order + 1)..samples.len() {
        let base = samples[i - order - 1];
        let mut weighted = 0i32;
        for (tap, &c) in coefficients[..order].iter().enumerate() {
            weighted =
                weighted.wrapping_add(i32::from(c).wrapping_mul(samples[i - 1 - tap] - base));
        }
        let prediction =
            base.wrapping_add(weighted.wrapping_add(PREDICTION_HALF) >> PREDICTION_SHIFT);
        let error = narrow_signed(samples[i].wrapping_sub(prediction), CODED_BITS);
        residuals[i] = error;
        let mut remaining = error;
        for tap in (0..order).rev() {
            if remaining == 0 {
                break;
            }
            let difference = base - samples[i - 1 - tap];
            let direction = difference.signum();
            let weight = (order - tap) as i32;
            if error > 0 {
                coefficients[tap] = coefficients[tap].wrapping_sub(direction as i16);
                remaining -= weight * ((direction * difference) >> PREDICTION_SHIFT);
                if remaining <= 0 {
                    break;
                }
            } else {
                coefficients[tap] = coefficients[tap].wrapping_add(direction as i16);
                remaining -= weight * ((-direction * difference) >> PREDICTION_SHIFT);
                if remaining >= 0 {
                    break;
                }
            }
        }
    }
}

fn write_predictor_header(w: &mut BitWriter, order: usize) {
    w.write(PREDICTION_SHIFT, 8);
    w.write((4 << 5) | order as u32, 8);
    let coefficients = prediction_coefficients(order);
    for &c in &coefficients[..order] {
        w.write(u32::from(c as u16), 16);
    }
}

fn fold_signed(value: i32) -> u32 {
    ((value as u32) << 1) ^ ((value >> 31) as u32)
}

fn rice_parameter(history: u32) -> u32 {
    let p = (32 - ((history >> HISTORY_FRACTION_BITS) + 3).leading_zeros()) - 1;
    p.min(RICE_LIMIT)
}

fn next_history(history: u32, symbol: u32, adjustment: u32) -> u32 {
    let h = HISTORY_MULTIPLIER
        .wrapping_mul(symbol.wrapping_add(adjustment))
        .wrapping_add(history)
        .wrapping_sub(HISTORY_MULTIPLIER.wrapping_mul(history) >> HISTORY_FRACTION_BITS);
    if symbol > HISTORY_CLAMP {
        HISTORY_CLAMP
    } else {
        h
    }
}

fn zero_run_parameters(history: u32) -> (u32, u32) {
    let parameter = history.leading_zeros() - 24 + ((history + 16) >> 6);
    let divisor = ((1u32 << parameter) - 1) & ((1 << RICE_LIMIT) - 1);
    (parameter, divisor)
}

fn entropy_value_bits(
    value: u32,
    divisor: u32,
    parameter: u32,
    escape_bits: u32,
    omit_unit: bool,
) -> usize {
    let prefix = value / divisor;
    if prefix >= ESCAPE_PREFIX {
        return (ESCAPE_PREFIX + escape_bits) as usize;
    }
    let bits = prefix as usize + 1;
    if omit_unit && parameter == 1 {
        return bits;
    }
    if value - prefix * divisor == 0 {
        bits + parameter as usize - 1
    } else {
        bits + parameter as usize
    }
}

fn entropy_bit_count(residuals: &[i32], channel_bits: u32) -> usize {
    let mut history = HISTORY_START;
    let mut adjustment = 0u32;
    let mut bits = 0usize;
    let mut i = 0;
    while i < residuals.len() {
        let parameter = rice_parameter(history);
        let divisor = (1u32 << parameter) - 1;
        let symbol = fold_signed(residuals[i]).wrapping_sub(adjustment);
        bits += entropy_value_bits(symbol, divisor, parameter, channel_bits, true);
        i += 1;
        history = next_history(history, symbol, adjustment);
        adjustment = 0;
        if history.wrapping_mul(4) >= HISTORY_ZERO_THRESHOLD || i == residuals.len() {
            continue;
        }
        let (zp, zd) = zero_run_parameters(history);
        let mut run = 0u32;
        while i < residuals.len() && residuals[i] == 0 {
            run += 1;
            i += 1;
        }
        bits += entropy_value_bits(run, zd, zp, 16, false);
        if run < 0xffff {
            adjustment = 1;
        }
        history = 0;
    }
    bits
}

fn write_entropy_value(
    w: &mut BitWriter,
    value: u32,
    divisor: u32,
    parameter: u32,
    escape_bits: u32,
    omit_unit: bool,
) {
    let prefix = value / divisor;
    if prefix >= ESCAPE_PREFIX {
        w.write((1 << ESCAPE_PREFIX) - 1, ESCAPE_PREFIX);
        w.write(value, escape_bits);
        return;
    }
    // prefix unos seguidos de un cero
    w.write(((1u32 << prefix) - 1) << 1, prefix + 1);
    if omit_unit && parameter == 1 {
        return;
    }
    let remainder = value - prefix * divisor;
    if remainder == 0 {
        w.write(0, parameter - 1);
    } else {
        w.write(remainder + 1, parameter);
    }
}

fn write_entropy_block(w: &mut BitWriter, residuals: &[i32]) {
    let mut history = HISTORY_START;
    let mut adjustment = 0u32;
    let mut i = 0;
    while i < residuals.len() {
        let parameter = rice_parameter(history);
        let divisor = (1u32 << parameter) - 1;
        let symbol = fold_signed(residuals[i]).wrapping_sub(adjustment);
        write_entropy_value(w, symbol, divisor, parameter, CODED_BITS, true);
        i += 1;
        history = next_history(history, symbol, adjustment);
        adjustment = 0;
        if history.wrapping_mul(4) >= HISTORY_ZERO_THRESHOLD || i == residuals.len() {
            continue;
        }
        let (zp, zd) = zero_run_parameters(history);
        let mut run = 0u32;
        while i < residuals.len() && residuals[i] == 0 {
            run += 1;
            i += 1;
        }
        write_entropy_value(w, run, zd, zp, 16, false);
        if run < 0xffff {
            adjustment = 1;
        }
        history = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::audio::{AudioCodec, AudioFormat, create_decoder};

    const N: usize = 352;

    fn sine() -> Vec<i16> {
        (0..N)
            .flat_map(|i| {
                let t = i as f64 / 44100.0;
                [
                    ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16000.0) as i16,
                    ((2.0 * std::f64::consts::PI * 659.25 * t).sin() * 12000.0) as i16,
                ]
            })
            .collect()
    }

    fn noise(seed: u32) -> Vec<i16> {
        let mut state = seed;
        (0..N * 2)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 16) as u16 as i16
            })
            .collect()
    }

    fn transient() -> Vec<i16> {
        (0..N)
            .flat_map(|i| {
                if i % 20 < 10 {
                    [20000, -10000]
                } else {
                    [-20000, 10000]
                }
            })
            .collect()
    }

    fn square() -> Vec<i16> {
        (0..N)
            .flat_map(|i| {
                if i % 16 < 8 {
                    [i16::MAX, i16::MIN]
                } else {
                    [i16::MIN, i16::MAX]
                }
            })
            .collect()
    }

    fn music(frames: usize) -> Vec<i16> {
        let mut state = 12345u32;
        (0..frames)
            .flat_map(|i| {
                let t = i as f64 / 44100.0;
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let hiss = ((state >> 16) as i16 as f64) * 0.01;
                let tau = 2.0 * std::f64::consts::PI;
                let env = 0.5 + 0.5 * (tau * 3.0 * t).sin();
                let l = (tau * 220.0 * t).sin() * 8000.0 * env
                    + (tau * 880.0 * t).sin() * 3000.0
                    + (tau * 3520.0 * t).sin() * 800.0
                    + hiss;
                let r = (tau * 330.0 * t).sin() * 7000.0
                    + (tau * 1320.0 * t).sin() * 2500.0 * env
                    + hiss;
                [l as i16, r as i16]
            })
            .collect()
    }

    fn decoder() -> Box<dyn crate::media::audio::Decoder> {
        create_decoder(&AudioFormat {
            codec: AudioCodec::Alac,
            sample_rate: 44100,
            channels: 2,
            frames_per_packet: 352,
        })
        .expect("decodificador ALAC")
    }

    // Codifica y decodifica; devuelve (tamano total codificado, muestras decodificadas).
    fn round_trip(signal: &[i16]) -> (usize, Vec<i16>) {
        let mut enc = AlacEncoder::new(N);
        let mut dec = decoder();
        let mut out = Vec::new();
        let mut decoded = Vec::new();
        let mut buf = vec![0i16; N * 2 * 4];
        let mut total = 0;
        for chunk in signal.chunks(N * 2) {
            enc.encode(chunk, &mut out).unwrap();
            total += out.len();
            let frames = dec.decode(&out, &mut buf).unwrap();
            decoded.extend_from_slice(&buf[..frames * 2]);
        }
        (total, decoded)
    }

    fn assert_exact(name: &str, signal: &[i16]) -> usize {
        let (size, decoded) = round_trip(signal);
        assert_eq!(decoded.len(), signal.len(), "{name}: longitud");
        if let Some(pos) = decoded.iter().zip(signal).position(|(a, b)| a != b) {
            panic!(
                "{name}: difiere en la muestra {pos}: {} != {}",
                decoded[pos], signal[pos]
            );
        }
        println!("{name}: {} bytes por {} bytes PCM", size, signal.len() * 2);
        size
    }

    #[test]
    fn silence_round_trip() {
        let size = assert_exact("silencio", &vec![0i16; N * 2]);
        assert!(size < 40);
    }

    #[test]
    fn sine_round_trip_and_compresses() {
        let size = assert_exact("seno", &sine());
        let verbatim = (3 + 4 + 12 + 4 + 32 + N * 32 + 3).div_ceil(8);
        assert!(size < verbatim, "{size} >= {verbatim}");
    }

    #[test]
    fn noise_falls_back_to_verbatim() {
        let signal = noise(0x4d595df4);
        let size = assert_exact("ruido", &signal);
        assert_eq!(size, (3 + 4 + 12 + 4 + 32 + N * 32 + 3).div_ceil(8));
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        enc.encode(&signal, &mut out).unwrap();
        assert_eq!(&hex_prefix(&out)[..12], "200012000002");
    }

    fn hex_prefix(out: &[u8]) -> String {
        out[..7].iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn square_round_trip() {
        assert_exact("cuadrada", &square());
    }

    #[test]
    fn transient_round_trip_fits_rfc2198() {
        let signal = transient();
        assert_exact("transitorio", &signal);
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        enc.encode(&signal, &mut out).unwrap();
        assert!(out.len() <= 0x3ff);
    }

    #[test]
    fn music_second_round_trip() {
        assert_exact("musica", &music(125 * N));
    }

    #[test]
    fn compressed_header_and_predictor_layout() {
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        enc.encode(&sine(), &mut out).unwrap();
        assert!(out.len() < 1024);
        assert!(2 * out.len() + 5 <= 1472 - 36);
        assert_eq!(out[0], 0x20);
        let flags = (u16::from(out[2]) << 8 | u16::from(out[3])) >> 9 & 0xf;
        assert_eq!(flags, 8);
        assert_eq!(&hex_prefix(&out)[..12], "200010000002");
        let mut pos = 55usize;
        let mut read = |bits: usize| {
            let mut v = 0u32;
            for _ in 0..bits {
                v = (v << 1) | u32::from(out[pos / 8] >> (7 - pos % 8) & 1);
                pos += 1;
            }
            v
        };
        assert_eq!(read(8), STEREO_SHIFT);
        assert!(read(8) <= 1 << STEREO_SHIFT);
        for _ in 0..2 {
            assert_eq!(read(8), PREDICTION_SHIFT);
            let order = (read(8) & 0x1f) as usize;
            assert!((MIN_ORDER..=MAX_ORDER).contains(&order));
            let expected = prediction_coefficients(order);
            for &c in &expected[..order] {
                assert_eq!(read(16) as u16 as i16, c);
            }
        }
    }

    #[test]
    fn verbatim_header_matches_reference() {
        let mut out = Vec::new();
        encode_verbatim(&mut out, &sine(), N);
        assert_eq!(&hex_prefix(&out)[..12], "200012000002");
        assert_eq!(out.len(), (23 + 32 + N * 32 + 3).div_ceil(8));
        assert_eq!(out[0], 0x20);
        assert_eq!(out[1], 0x00);
    }

    #[test]
    fn entropy_count_matches_writer() {
        let signals = [sine(), transient(), vec![0i16; N * 2], noise(7)];
        for signal in &signals {
            for ch in 0..2 {
                let samples: Vec<i32> = signal.chunks(2).map(|f| i32::from(f[ch])).collect();
                let mut residuals = vec![0i32; N];
                for order in MIN_ORDER..=MAX_ORDER {
                    predict_residuals(&samples, &mut residuals, order);
                    let mut out = Vec::new();
                    let mut w = BitWriter::new(&mut out);
                    write_entropy_block(&mut w, &residuals);
                    let written = w.out.len() * 8 + w.nbits as usize;
                    assert_eq!(entropy_bit_count(&residuals, CODED_BITS), written);
                }
            }
        }
    }

    #[test]
    fn rejects_wrong_length() {
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        assert!(enc.encode(&[0i16; 10], &mut out).is_err());
    }

    #[test]
    fn reuses_output_capacity() {
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        let signal = music(N);
        enc.encode(&signal, &mut out).unwrap();
        let (ptr, cap) = (out.as_ptr(), out.capacity());
        for _ in 0..50 {
            enc.encode(&signal, &mut out).unwrap();
        }
        assert_eq!((out.as_ptr(), out.capacity()), (ptr, cap));
    }

    #[test]
    #[ignore]
    fn perf_encode_time() {
        let mut enc = AlacEncoder::new(N);
        let mut out = Vec::new();
        for (name, signal) in [("musica", music(N)), ("ruido", noise(1)), ("seno", sine())] {
            for _ in 0..100 {
                enc.encode(&signal, &mut out).unwrap();
            }
            let iters = 5000;
            let start = std::time::Instant::now();
            for _ in 0..iters {
                enc.encode(&signal, &mut out).unwrap();
            }
            let us = start.elapsed().as_secs_f64() * 1e6 / f64::from(iters);
            println!("{name}: {us:.1} us por paquete");
        }
    }
}
