// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

use super::bits::BitReader;
use super::config::{FRAME_LENGTH, SwbInfo};
use super::huffman::Codebooks;
use super::tns::Tns;
use crate::{Error, Result};

pub const MAX_BANDS: usize = 64;

const ZERO_BT: u8 = 0;
const ESC_BT: u8 = 11;
const NOISE_BT: u8 = 13;
const INTENSITY_BT2: u8 = 14;
const INTENSITY_BT: u8 = 15;
const NOISE_PRE: i32 = 256;
const NOISE_PRE_BITS: u32 = 9;
const NOISE_OFFSET: i32 = 90;
const SCALE_DIFF_ZERO: i32 = 60;
const POW_SF2_ZERO: i32 = 200;

pub struct Channel {
    pub coeffs: [f32; FRAME_LENGTH],
    pub band_type: [u8; MAX_BANDS],
    pub sf: [f32; MAX_BANDS],
    pub sfo: [i32; MAX_BANDS],
    pub max_sfb: usize,
    pub tns: Tns,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            coeffs: [0.0; FRAME_LENGTH],
            band_type: [0; MAX_BANDS],
            sf: [0.0; MAX_BANDS],
            sfo: [0; MAX_BANDS],
            max_sfb: 0,
            tns: Tns::default(),
        }
    }

    fn reset_on_error(&mut self) {
        self.sfo = [0; MAX_BANDS];
        self.tns.present = false;
    }
}

fn media(msg: &str) -> Error {
    Error::Media(msg.into())
}

pub fn decode_ics_info(br: &mut BitReader, swb: &SwbInfo) -> Result<usize> {
    let max_sfb = br.read(6) as usize;
    if max_sfb > swb.num_swb {
        return Err(media("número de bandas AAC fuera de rango"));
    }
    Ok(max_sfb)
}

fn decode_band_types(br: &mut BitReader, ch: &mut Channel) -> Result<()> {
    let max_sfb = ch.max_sfb;
    let mut k = 0;
    while k < max_sfb {
        let mut sect_end = k;
        let sect_band_type = br.read(4) as u8;
        if sect_band_type == 12 {
            return Err(media("tipo de banda AAC no válido"));
        }
        loop {
            let incr = br.read(5) as usize;
            sect_end += incr;
            if br.overrun() {
                return Err(media("datos AAC truncados"));
            }
            if sect_end > max_sfb {
                return Err(media("sección AAC fuera del número de bandas"));
            }
            if incr != 31 {
                break;
            }
        }
        for slot in &mut ch.band_type[k..sect_end] {
            *slot = sect_band_type;
        }
        k = sect_end;
    }
    Ok(())
}

fn scale_diff(br: &mut BitReader, cb: &Codebooks) -> Result<i32> {
    cb.scalefactor
        .decode(br)
        .map(|v| v as i32 - SCALE_DIFF_ZERO)
        .ok_or_else(|| media("código de factor de escala no válido"))
}

fn decode_scalefactors(
    br: &mut BitReader,
    cb: &Codebooks,
    ch: &mut Channel,
    global_gain: i32,
) -> Result<()> {
    let mut offset = [global_gain, global_gain - NOISE_OFFSET, 0];
    let mut noise_flag = 1;
    for sfb in 0..ch.max_sfb {
        match ch.band_type[sfb] {
            ZERO_BT => ch.sfo[sfb] = 0,
            INTENSITY_BT | INTENSITY_BT2 => {
                offset[2] += scale_diff(br, cb)?;
                ch.sfo[sfb] = offset[2].clamp(-155, 100) - 100;
            }
            NOISE_BT => {
                if noise_flag > 0 {
                    offset[1] += br.read(NOISE_PRE_BITS) as i32 - NOISE_PRE;
                } else {
                    offset[1] += scale_diff(br, cb)?;
                }
                noise_flag -= 1;
                ch.sfo[sfb] = offset[1].clamp(-100, 155);
            }
            _ => {
                offset[0] += scale_diff(br, cb)?;
                if !(0..=255).contains(&offset[0]) {
                    return Err(media("factor de escala AAC fuera de rango"));
                }
                ch.sfo[sfb] = offset[0] - 100;
            }
        }
        if br.overrun() {
            return Err(media("datos AAC truncados"));
        }
    }
    Ok(())
}

fn pow2sf(cb: &Codebooks, idx: i32) -> f32 {
    usize::try_from(idx)
        .ok()
        .and_then(|i| cb.pow2sf.get(i))
        .copied()
        .unwrap_or(0.0)
}

fn dequant_scalefactors(cb: &Codebooks, ch: &mut Channel) {
    for sfb in 0..ch.max_sfb {
        let sfo = ch.sfo[sfb];
        ch.sf[sfb] = match ch.band_type[sfb] {
            ZERO_BT => 0.0,
            INTENSITY_BT | INTENSITY_BT2 => pow2sf(cb, -sfo - 100 + POW_SF2_ZERO),
            _ => -pow2sf(cb, sfo + POW_SF2_ZERO),
        };
    }
}

fn lcg_random(prev: u32) -> u32 {
    prev.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn with_sign(v: f32, negative: bool) -> f32 {
    if negative { -v } else { v }
}

fn signed_mag(br: &mut BitReader, cb: &Codebooks, m: usize, sf: f32) -> f32 {
    if m == 0 {
        return 0.0;
    }
    let v = cb.pow43[m] * sf;
    with_sign(v, br.read_bit() == 1)
}

pub fn decode_band(
    br: &mut BitReader,
    cb: &Codebooks,
    band_type: u8,
    sf: f32,
    out: &mut [f32],
) -> Result<()> {
    let vlc = &cb.spectral[usize::from(band_type) - 1];
    let invalid = || media("código espectral AAC no válido");
    match band_type {
        1 | 2 => {
            for q in out.chunks_exact_mut(4) {
                let idx = vlc.decode(br).ok_or_else(invalid)?;
                q[0] = ((idx / 27) as f32 - 1.0) * sf;
                q[1] = ((idx / 9 % 3) as f32 - 1.0) * sf;
                q[2] = ((idx / 3 % 3) as f32 - 1.0) * sf;
                q[3] = ((idx % 3) as f32 - 1.0) * sf;
            }
        }
        3 | 4 => {
            for q in out.chunks_exact_mut(4) {
                let idx = vlc.decode(br).ok_or_else(invalid)?;
                let mags = [idx / 27, idx / 9 % 3, idx / 3 % 3, idx % 3];
                for (o, m) in q.iter_mut().zip(mags) {
                    *o = signed_mag(br, cb, m, sf);
                }
            }
        }
        5 | 6 => {
            for q in out.chunks_exact_mut(2) {
                let idx = vlc.decode(br).ok_or_else(invalid)?;
                for (o, m) in q.iter_mut().zip([idx / 9, idx % 9]) {
                    *o = with_sign(cb.pow43[m.abs_diff(4)] * sf, m < 4);
                }
            }
        }
        7..=10 => {
            let dim = if band_type < 9 { 8 } else { 13 };
            for q in out.chunks_exact_mut(2) {
                let idx = vlc.decode(br).ok_or_else(invalid)?;
                for (o, m) in q.iter_mut().zip([idx / dim, idx % dim]) {
                    *o = signed_mag(br, cb, m, sf);
                }
            }
        }
        ESC_BT => {
            for q in out.chunks_exact_mut(2) {
                let idx = vlc.decode(br).ok_or_else(invalid)?;
                let mags = [idx / 17, idx % 17];
                let mut negative = [false; 2];
                for (s, m) in negative.iter_mut().zip(mags) {
                    if m != 0 {
                        *s = br.read_bit() == 1;
                    }
                }
                for ((o, m), s) in q.iter_mut().zip(mags).zip(negative) {
                    let n = if m == 16 {
                        let mut ones = 0u32;
                        while br.read_bit() == 1 {
                            ones += 1;
                            if ones > 8 {
                                return Err(media("desbordamiento de escape AAC"));
                            }
                        }
                        let b = ones + 4;
                        (1usize << b) + br.read(b) as usize
                    } else {
                        m
                    };
                    *o = with_sign(cb.pow43.get(n).copied().unwrap_or(0.0) * sf, s);
                }
            }
        }
        _ => return Err(media("libro de códigos AAC no válido")),
    }
    Ok(())
}

fn decode_spectrum(
    br: &mut BitReader,
    cb: &Codebooks,
    ch: &mut Channel,
    swb: &SwbInfo,
    rng: &mut u32,
) -> Result<()> {
    let offsets = swb.offsets;
    let end = usize::from(offsets[ch.max_sfb]);
    ch.coeffs[end..].fill(0.0);
    for sfb in 0..ch.max_sfb {
        let lo = usize::from(offsets[sfb]);
        let hi = usize::from(offsets[sfb + 1]);
        let band_type = ch.band_type[sfb];
        let sf = ch.sf[sfb];
        let out = &mut ch.coeffs[lo..hi];
        match band_type {
            ZERO_BT | INTENSITY_BT | INTENSITY_BT2 => out.fill(0.0),
            NOISE_BT => {
                let mut energy = 0.0f32;
                for c in out.iter_mut() {
                    *rng = lcg_random(*rng);
                    *c = *rng as i32 as f32;
                    energy += *c * *c;
                }
                let scale = sf / energy.sqrt();
                if scale.is_finite() {
                    for c in out.iter_mut() {
                        *c *= scale;
                    }
                } else {
                    out.fill(0.0);
                }
            }
            _ => decode_band(br, cb, band_type, sf, out)?,
        }
        if br.overrun() {
            return Err(media("datos AAC truncados"));
        }
    }
    Ok(())
}

// `common_max_sfb` is Some when the channel pair shares one ics_info.
pub fn decode_ics(
    br: &mut BitReader,
    cb: &Codebooks,
    ch: &mut Channel,
    swb: &SwbInfo,
    rng: &mut u32,
    common_max_sfb: Option<usize>,
) -> Result<()> {
    let r = decode_ics_inner(br, cb, ch, swb, rng, common_max_sfb);
    if r.is_err() {
        ch.reset_on_error();
    }
    r
}

fn decode_ics_inner(
    br: &mut BitReader,
    cb: &Codebooks,
    ch: &mut Channel,
    swb: &SwbInfo,
    rng: &mut u32,
    common_max_sfb: Option<usize>,
) -> Result<()> {
    let global_gain = br.read(8) as i32;
    ch.max_sfb = match common_max_sfb {
        Some(m) => m,
        None => decode_ics_info(br, swb)?,
    };
    ch.band_type.fill(0);
    decode_band_types(br, ch)?;
    decode_scalefactors(br, cb, ch, global_gain)?;
    dequant_scalefactors(cb, ch);
    ch.tns.present = br.read_bit() == 1;
    if ch.tns.present {
        ch.tns.parse(br)?;
    }
    decode_spectrum(br, cb, ch, swb, rng)
}
