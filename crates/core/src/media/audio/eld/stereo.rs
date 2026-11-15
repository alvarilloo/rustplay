// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

#![allow(clippy::needless_range_loop)]

use super::channel::{Channel, MAX_BANDS};
use super::config::SwbInfo;

const NOISE_BT: u8 = 13;
const INTENSITY_BT2: u8 = 14;
const INTENSITY_BT: u8 = 15;

pub fn apply_mid_side(
    ch0: &mut Channel,
    ch1: &mut Channel,
    ms_mask: &[bool; MAX_BANDS],
    swb: &SwbInfo,
) {
    for sfb in 0..ch0.max_sfb {
        if ms_mask[sfb] && ch0.band_type[sfb] < NOISE_BT && ch1.band_type[sfb] < NOISE_BT {
            let lo = usize::from(swb.offsets[sfb]);
            let hi = usize::from(swb.offsets[sfb + 1]);
            for (a, b) in ch0.coeffs[lo..hi].iter_mut().zip(&mut ch1.coeffs[lo..hi]) {
                let t = *a - *b;
                *a += *b;
                *b = t;
            }
        }
    }
}

pub fn apply_intensity(
    ch0: &Channel,
    ch1: &mut Channel,
    ms_present: bool,
    ms_mask: &[bool; MAX_BANDS],
    swb: &SwbInfo,
) {
    for sfb in 0..ch1.max_sfb {
        let bt = ch1.band_type[sfb];
        if bt == INTENSITY_BT || bt == INTENSITY_BT2 {
            let mut c = -1.0 + 2.0 * f32::from(bt - 14);
            if ms_present && ms_mask[sfb] {
                c = -c;
            }
            let scale = c * ch1.sf[sfb];
            let lo = usize::from(swb.offsets[sfb]);
            let hi = usize::from(swb.offsets[sfb + 1]);
            for (d, s) in ch1.coeffs[lo..hi].iter_mut().zip(&ch0.coeffs[lo..hi]) {
                *d = *s * scale;
            }
        }
    }
}
