// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

use super::bits::BitReader;
use super::tables::*;

const LEAF: u16 = 0x8000;

pub struct Vlc {
    nodes: Vec<[u16; 2]>,
}

impl Vlc {
    fn build(codes: impl Iterator<Item = (u32, u8)>) -> Self {
        let mut nodes: Vec<[u16; 2]> = vec![[0, 0]];
        for (sym, (code, len)) in codes.enumerate() {
            let mut node = 0usize;
            for b in (0..len).rev() {
                let bit = ((code >> b) & 1) as usize;
                if b == 0 {
                    nodes[node][bit] = LEAF | sym as u16;
                } else {
                    let next = nodes[node][bit];
                    node = if next == 0 {
                        nodes.push([0, 0]);
                        let idx = nodes.len() - 1;
                        nodes[node][bit] = idx as u16;
                        idx
                    } else {
                        usize::from(next)
                    };
                }
            }
        }
        Self { nodes }
    }

    pub fn decode(&self, br: &mut BitReader) -> Option<usize> {
        let mut node = 0usize;
        loop {
            let bit = br.read_bit() as usize;
            let v = *self.nodes.get(node)?.get(bit)?;
            if v == 0 {
                return None;
            }
            if v & LEAF != 0 {
                return Some(usize::from(v & !LEAF));
            }
            node = usize::from(v);
        }
    }
}

pub struct Codebooks {
    pub scalefactor: Vlc,
    pub spectral: Vec<Vlc>,
    pub pow43: Vec<f32>,
    pub pow2sf: Vec<f32>,
}

fn pairs<'a, C: Copy + Into<u32>>(
    codes: &'a [C],
    bits: &'a [u8],
) -> impl Iterator<Item = (u32, u8)> + 'a {
    codes.iter().zip(bits.iter()).map(|(&c, &b)| (c.into(), b))
}

impl Codebooks {
    pub fn new() -> Self {
        let spectral = vec![
            Vlc::build(pairs(&SPECTRAL_CODES_1, &SPECTRAL_BITS_1)),
            Vlc::build(pairs(&SPECTRAL_CODES_2, &SPECTRAL_BITS_2)),
            Vlc::build(pairs(&SPECTRAL_CODES_3, &SPECTRAL_BITS_3)),
            Vlc::build(pairs(&SPECTRAL_CODES_4, &SPECTRAL_BITS_4)),
            Vlc::build(pairs(&SPECTRAL_CODES_5, &SPECTRAL_BITS_5)),
            Vlc::build(pairs(&SPECTRAL_CODES_6, &SPECTRAL_BITS_6)),
            Vlc::build(pairs(&SPECTRAL_CODES_7, &SPECTRAL_BITS_7)),
            Vlc::build(pairs(&SPECTRAL_CODES_8, &SPECTRAL_BITS_8)),
            Vlc::build(pairs(&SPECTRAL_CODES_9, &SPECTRAL_BITS_9)),
            Vlc::build(pairs(&SPECTRAL_CODES_10, &SPECTRAL_BITS_10)),
            Vlc::build(pairs(&SPECTRAL_CODES_11, &SPECTRAL_BITS_11)),
        ];
        let scalefactor = Vlc::build(pairs(&SCALEFACTOR_CODE, &SCALEFACTOR_BITS));
        let pow43 = (0..8192)
            .map(|n| f64::from(n).powf(4.0 / 3.0) as f32)
            .collect();
        let pow2sf = (0..428)
            .map(|i| 2f64.powf(f64::from(i - 200) / 4.0) as f32)
            .collect();
        Self {
            scalefactor,
            spectral,
            pow43,
            pow2sf,
        }
    }
}
