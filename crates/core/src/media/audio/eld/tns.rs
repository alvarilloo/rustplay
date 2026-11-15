// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

#![allow(clippy::excessive_precision)]

use super::bits::BitReader;
use super::config::SwbInfo;
use crate::{Error, Result};

const TNS_MAX_ORDER: usize = 20;
const TNS_MAX_FILTERS: usize = 3;
const TNS_ELD_MAX_ORDER: usize = 12;

const TMP2_MAP_0_3: [f32; 8] = [
    0.0,
    -0.43388373,
    -0.78183150,
    -0.97492790,
    0.98480773,
    0.86602539,
    0.64278758,
    0.34202015,
];
const TMP2_MAP_0_4: [f32; 16] = [
    0.0,
    -0.20791170,
    -0.40673664,
    -0.58778524,
    -0.74314481,
    -0.86602539,
    -0.95105654,
    -0.99452192,
    0.99573416,
    0.96182561,
    0.89516330,
    0.79801720,
    0.67369562,
    0.52643216,
    0.36124167,
    0.18374951,
];
const TMP2_MAP_1_3: [f32; 4] = [0.0, -0.43388373, 0.64278758, 0.34202015];
const TMP2_MAP_1_4: [f32; 8] = [
    0.0,
    -0.20791170,
    -0.40673664,
    -0.58778524,
    0.67369562,
    0.52643216,
    0.36124167,
    0.18374951,
];

fn tmp2_map(idx: usize) -> &'static [f32] {
    match idx {
        0 => &TMP2_MAP_0_3,
        1 => &TMP2_MAP_0_4,
        2 => &TMP2_MAP_1_3,
        _ => &TMP2_MAP_1_4,
    }
}

#[derive(Clone, Copy, Default)]
struct Filter {
    length: usize,
    order: usize,
    direction: bool,
    coef: [f32; TNS_ELD_MAX_ORDER],
}

#[derive(Default)]
pub struct Tns {
    pub present: bool,
    n_filt: usize,
    filters: [Filter; TNS_MAX_FILTERS],
}

impl Tns {
    pub fn parse(&mut self, br: &mut BitReader) -> Result<()> {
        self.n_filt = 0;
        let n_filt = br.read(2) as usize;
        if n_filt == 0 {
            return Ok(());
        }
        let coef_res = br.read_bit() as usize;
        for filt in 0..n_filt {
            let f = &mut self.filters[filt];
            f.length = br.read(6) as usize;
            f.order = br.read(5) as usize;
            if f.order > TNS_ELD_MAX_ORDER {
                f.order = 0;
                self.present = false;
                return Err(Error::Media("orden de filtro TNS excesivo".into()));
            }
            if f.order > 0 {
                f.direction = br.read_bit() == 1;
                let compress = br.read_bit() as usize;
                let coef_len = (coef_res + 3 - compress) as u32;
                let map = tmp2_map(2 * compress + coef_res);
                for c in f.coef.iter_mut().take(f.order) {
                    *c = map.get(br.read(coef_len) as usize).copied().unwrap_or(0.0);
                }
            }
        }
        self.n_filt = n_filt;
        Ok(())
    }

    pub fn apply(&self, coef: &mut [f32], swb: &SwbInfo, max_sfb: usize) {
        let mmm = swb.tns_max_bands.min(max_sfb);
        if mmm == 0 {
            return;
        }
        let mut bottom = swb.num_swb;
        for f in &self.filters[..self.n_filt] {
            let top = bottom;
            bottom = top.saturating_sub(f.length);
            let order = f.order;
            if order == 0 {
                continue;
            }
            let lpc = compute_lpc(&f.coef[..order]);
            let start = usize::from(swb.offsets[bottom.min(mmm)]);
            let end = usize::from(swb.offsets[top.min(mmm)]);
            if end <= start {
                continue;
            }
            let size = end - start;
            let Some(band) = coef.get_mut(start..end) else {
                continue;
            };
            if f.direction {
                for m in 0..size {
                    let pos = size - 1 - m;
                    for i in 1..=m.min(order) {
                        band[pos] -= band[pos + i] * lpc[i - 1];
                    }
                }
            } else {
                for m in 0..size {
                    for i in 1..=m.min(order) {
                        band[m] -= band[m - i] * lpc[i - 1];
                    }
                }
            }
        }
    }
}

fn compute_lpc(autoc: &[f32]) -> [f32; TNS_MAX_ORDER] {
    let mut lpc = [0.0f32; TNS_MAX_ORDER];
    for i in 0..autoc.len() {
        let r = -autoc[i];
        lpc[i] = r;
        for j in 0..(i + 1) >> 1 {
            let f = lpc[j];
            let b = lpc[i - 1 - j];
            lpc[j] = f + r * b;
            lpc[i - 1 - j] = b + r * f;
        }
    }
    lpc
}
