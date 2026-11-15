// Sender side of FairPlay SAP (FPLY v3): m1/m3 generation, m4 check and ekey wrapping.
// The wire format and the proprietary hash follow the public reverse-engineering work
// behind UxPlay/playfair and the doubletake sender.

use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
use sha1::Sha1;

use super::sap_msg_tables::{INVERSE_SBOX, MESSAGE_IV, MIDDLE_KEYS, ROUND_KEY_0, ROUND_KEY_10};
use super::sap_tables::{
    FIRST_INPUT_MASK, FIRST_TABLES, MIX_BASES, SECOND_OUTPUT_MASK, SECOND_TABLES,
    SUBSTITUTION_BASES,
};
use crate::{Error, Result};

type Lookup = (u8, u8, u8);

pub struct NetworkTables {
    pub round_substitution: [[Lookup; 16]; 9],
    pub mix_columns: [[Lookup; 4]; 4],
    pub final_substitution: [Lookup; 16],
}

fn substitute(l: Lookup) -> impl Fn(u8) -> u8 {
    move |v| SUBSTITUTION_BASES[usize::from(l.0)][usize::from(v ^ l.1)] ^ l.2
}

fn mix(l: Lookup, v: u8) -> u8 {
    MIX_BASES[usize::from(l.0)][usize::from(v ^ l.1)] ^ l.2
}

const M1_PAYLOAD: [u8; 4] = [0x02, 0x00, 0x03, 0xbb];
const M3_LABEL: [u8; 3] = [0x8f, 0x1a, 0x9c];

const INITIAL_SESSION_KEY: [u8; 16] = [
    0xdc, 0xdc, 0xf3, 0xb9, 0x0b, 0x74, 0xdc, 0xfb, 0x86, 0x7f, 0xf7, 0x60, 0x16, 0x72, 0x90, 0x51,
];
const KDF_PREFIX: [u8; 17] = [
    0xfa, 0x9c, 0xad, 0x4d, 0x4b, 0x68, 0x26, 0x8c, 0x7f, 0xf3, 0x88, 0x99, 0xde, 0x92, 0x2e, 0x95,
    0x1e,
];
const KDF_SUFFIX: [u8; 17] = [
    0xec, 0x4e, 0x27, 0x5e, 0xfd, 0xf2, 0xe8, 0x30, 0x97, 0xae, 0x70, 0xfb, 0xe0, 0x00, 0x3f, 0x1c,
    0x39,
];
const DESCRIPTOR_PREFIX: [u8; 17] = [
    0xa0, 0x44, 0x9c, 0x4d, 0x09, 0xe4, 0xbd, 0x7f, 0x6e, 0xc5, 0xd0, 0xcc, 0x35, 0x9d, 0xa7, 0x46,
    0x7a,
];
const DESCRIPTOR_SUFFIX: [u8; 17] = [
    0x97, 0xb5, 0x0f, 0x84, 0xe2, 0x15, 0x5a, 0x9c, 0x24, 0x99, 0x1c, 0xf4, 0x3a, 0x09, 0x63, 0x55,
    0x47,
];
const FIXED_BLOCK: [u8; 16] = [
    0xaf, 0xc2, 0x2b, 0xa0, 0x49, 0xef, 0xfc, 0xfb, 0xfe, 0x67, 0xac, 0x5e, 0xbe, 0xf6, 0xfb, 0xcb,
];
const FIRST_POSITION_MAP: [usize; 16] = [0, 5, 10, 15, 4, 9, 14, 3, 8, 13, 2, 7, 12, 1, 6, 11];
const SECOND_POSITION_MAP: [usize; 16] = [0, 13, 10, 7, 4, 1, 14, 11, 8, 5, 2, 15, 12, 9, 6, 3];

// ---- modified MD5 ----

const MD5_SHIFT: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_CONSTANT: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

#[derive(Clone, Copy)]
enum Mutation {
    Swap,
    Cycle,
    Kdf,
}

fn md5_compress(state: [u32; 4], block: &[u8], mutation: Mutation) -> [u32; 4] {
    let mut message = [0u32; 16];
    for (i, word) in message.iter_mut().enumerate() {
        *word = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    let [mut a, mut b, mut c, mut d] = state;
    for round in 0..64usize {
        let (f, word) = match round {
            0..=15 => ((b & c) | (!b & d), round),
            16..=31 => ((d & b) | (!d & c), (5 * round + 1) & 15),
            32..=47 => (b ^ c ^ d, (3 * round + 5) & 15),
            _ => (c ^ (b | !d), (7 * round) & 15),
        };
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(MD5_CONSTANT[round])
            .wrapping_add(message[word])
            .rotate_left(MD5_SHIFT[round]);
        (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
        if round == 31 {
            mutate_message(&mut message, [a, b, c, d], mutation);
        }
    }
    [
        state[0].wrapping_add(a),
        state[1].wrapping_add(b),
        state[2].wrapping_add(c),
        state[3].wrapping_add(d),
    ]
}

fn mutate_message(message: &mut [u32; 16], [a, b, c, d]: [u32; 4], mutation: Mutation) {
    let nib = |v: u32, shift: u32| ((v >> shift) & 15) as usize;
    match mutation {
        Mutation::Swap => {
            let idx = [
                nib(a, 0),
                nib(b, 0),
                nib(c, 0),
                nib(d, 0),
                nib(a, 4),
                nib(b, 4),
                nib(c, 4),
                nib(d, 4),
            ];
            for (i, &j) in idx.iter().enumerate() {
                message.swap(i, j);
            }
        }
        Mutation::Cycle => {
            let idx = [
                nib(a, 0),
                nib(b, 0),
                nib(c, 0),
                nib(d, 0),
                nib(a, 4),
                nib(b, 4),
                nib(c, 4),
                nib(d, 4),
            ];
            let first = message[idx[0]];
            for i in 0..idx.len() - 1 {
                message[idx[i]] = message[idx[i + 1]];
            }
            message[idx[idx.len() - 1]] = first;
        }
        Mutation::Kdf => {
            message.swap(nib(a, 0), nib(b, 0));
            message.swap(nib(c, 0), nib(d, 0));
            for shift in [4, 8, 12] {
                message.swap(nib(a, shift), nib(b, shift));
            }
        }
    }
}

fn words_le(input: &[u8; 16]) -> [u32; 4] {
    std::array::from_fn(|i| {
        u32::from_le_bytes([
            input[i * 4],
            input[i * 4 + 1],
            input[i * 4 + 2],
            input[i * 4 + 3],
        ])
    })
}

fn words_be(words: [u32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, w) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
    }
    out
}

// ---- SAP hash ----

trait W8 {
    fn add(self, o: u8) -> u8;
    fn sub(self, o: u8) -> u8;
    fn mul(self, o: u8) -> u8;
}

impl W8 for u8 {
    fn add(self, o: u8) -> u8 {
        self.wrapping_add(o)
    }
    fn sub(self, o: u8) -> u8 {
        self.wrapping_sub(o)
    }
    fn mul(self, o: u8) -> u8 {
        self.wrapping_mul(o)
    }
}

fn rot(input: u8, count: u8) -> u8 {
    if count == 0 {
        0
    } else {
        input.rotate_left(u32::from(count) & 7)
    }
}

fn neg(v: u8) -> u8 {
    v.wrapping_neg()
}

fn maj(a: u8, b: u8, c: u8) -> u8 {
    a ^ ((a ^ b) & (a ^ c))
}

fn sel(mask: u8, if_set: u8, if_clear: u8) -> u8 {
    if_clear ^ ((if_set ^ if_clear) & mask)
}

fn sq(v: u8) -> u8 {
    v.wrapping_mul(v)
}

fn cube(v: u8) -> u8 {
    v.wrapping_mul(v).wrapping_mul(v)
}

const SEED: [u8; 21] = [
    0xed, 0x25, 0xd1, 0xbb, 0xbc, 0x27, 0x9f, 0x02, 0xa2, 0xa9, 0x11, 0x00, 0x0c, 0xb3, 0x52, 0xc0,
    0xbd, 0xe3, 0x1b, 0x49, 0xc7,
];

fn wide_seed(input: u8, count: u8) -> u8 {
    if count == 0 {
        return SEED[0];
    }
    let x = usize::from(input);
    let c = u32::from(count);
    SEED[((x << c) | (x >> (8 - c))) % SEED.len()]
}

struct SapState {
    hash: [u8; 20],
    matrix: [u8; 35],
    aux: [u8; 10],
    work: [u8; 210],
}

impl SapState {
    fn new() -> Self {
        Self {
            hash: [
                0x96, 0x5f, 0xc6, 0x53, 0xf8, 0x46, 0xcc, 0x18, 0xdf, 0xbe, 0xb2, 0xf8, 0x38, 0x62,
                0xec, 0x22, 0x93, 0xd1, 0x20, 0x8f,
            ],
            matrix: [
                0x43, 0x54, 0x62, 0x7a, 0x18, 0xc3, 0xd6, 0xb3, 0x9a, 0x56, 0xf6, 0x1c, 0x14, 0x3f,
                0x0c, 0x1d, 0x3b, 0x36, 0x83, 0xb1, 0x39, 0x51, 0x4a, 0xaa, 0x09, 0x3e, 0xfe, 0x44,
                0xaf, 0xde, 0xc3, 0x20, 0x9d, 0x42, 0xb8,
            ],
            aux: [0; 10],
            work: [0; 210],
        }
    }

    fn hi(&self, i: u8) -> u8 {
        self.hash[usize::from(i % 20)]
    }
    fn h(&self, i: usize) -> u8 {
        self.hi(self.work[i])
    }
    fn m(&self, i: usize) -> u8 {
        self.matrix[usize::from(self.work[i] % 35)]
    }
    fn s(&self, i: usize) -> u8 {
        si(self.work[i])
    }
    fn ma(&self, i: usize) -> u8 {
        self.matrix[usize::from(self.aux[i] % 35)]
    }
}

fn si(i: u8) -> u8 {
    SEED[usize::from(i % 21)]
}

fn sap_hash(block: &[u8]) -> [u8; 16] {
    let mut st = SapState::new();
    for i in 0..210 {
        st.work[i] = block[(i & 63) ^ 3];
    }
    for i in 0u32..840 {
        let x = st.work[(i.wrapping_sub(155) % 210) as usize];
        let y = st.work[(i.wrapping_sub(57) % 210) as usize];
        let z = st.work[(i.wrapping_sub(13) % 210) as usize];
        let w = st.work[(i % 210) as usize];
        st.work[(i % 210) as usize] = y
            .rotate_left(5)
            .add(z.rotate_left(3) ^ w)
            .sub(x.rotate_left(7));
    }
    st.nonlinear_circuit();

    let mut out = [0u8; 16];
    out[..3].copy_from_slice(&st.aux[..3]);
    out[4..11].copy_from_slice(&st.aux[3..]);
    for v in out.iter_mut() {
        *v = v.add(0xe1);
    }
    out[3] = 0x3d;
    out[11] = 0x3c;
    out[10] ^= st.aux[3] ^ 133;

    for (i, &w) in st.work.iter().enumerate() {
        let mut value = w;
        if i < st.matrix.len() {
            value ^= st.matrix[i];
        }
        if i < st.hash.len() {
            value ^= st.hash[i];
        }
        out[i & 15] ^= value;
    }
    for i in 0..256usize {
        out[i & 15] ^= out[(i + 16 - 7) & 15].rotate_left(1)
            ^ out[(i + 16 - 5) & 15].rotate_left(6)
            ^ out[(i + 16 - 1) & 15].rotate_left(5);
    }
    out
}

impl SapState {
    // Every expression keeps the operator grouping of the original (Go precedence: `*`, `/`,
    // `<<`, `>>`, `&`, `&^` bind tighter than `+`, `-`, `|`, `^`), and all arithmetic wraps as bytes.
    #[allow(clippy::too_many_lines, clippy::precedence)]
    fn nonlinear_circuit(&mut self) {
        let st = self;
        st.matrix[12] = 0x14u8.add(sel(92, st.work[64], st.work[99] / 3) & wide_seed(st.s(206), 4));
        st.work[4] = 2u8.mul(sq(st.work[99] / 5));
        st.work[153] ^= sq(st.m(203)).mul(st.work[190]);
        st.hash[3] = 0x13 ^ ((st.s(205) >> 1) & 0x10);
        st.work[33] = st.work[33].sub(st.s(36) & !9);
        st.aux[5] = ((st.m(67) & !2) | 1 | ((st.h(181) >> 6) & 2) | (st.hash[3] & 0x10)).sub(15);
        st.matrix[12] = 0x07;
        st.work[2] = st.work[2].sub(64);
        st.hash[19] = st.s(58);
        st.aux[4] = 92u8.sub(st.m(32));
        st.aux[9] = st.m(15).add(0x9e);
        st.work[34] = st.work[34].add(si(st.aux[9]) / 5);
        st.hash[19] = st.hash[19].add(0xe6 ^ ((st.hi(st.aux[9]) >> 1) & 0x66));
        st.work[15] ^= 3u8
            .mul(rot(st.work[72], neg(st.s(190)) & 7))
            .sub(9u8.mul(st.s(126)));
        st.hash[15] ^= cube(st.m(181));
        st.matrix[4] ^= st.work[202] / 3;
        st.matrix[1] = st.matrix[1].add(cube(maj(92u8.sub(st.hi(st.aux[4])), !st.work[105], 0xc6)));
        st.hash[19] ^=
            ((((224 | (st.s(92) & 27)) as usize * usize::from(st.m(41))) / 3) & 0xff) as u8;
        st.work[140] = st.work[140].add(rot(92, neg(st.work[5]) & 7));
        st.matrix[12] = st.matrix[12].add(maj(!st.work[4] ^ st.m(12), st.work[182], 192));
        st.work[36] = st.work[36].add(125);
        st.work[124] = maj(maj(st.work[138], st.hash[15], 74), st.h(43), 95).rotate_left(4);
        let aux_hash = st.hi(st.aux[9]);
        st.aux[1] = 0x4c & !(aux_hash & (st.s(68) << 1));
        st.aux[2] = 222u8.sub(maj(
            ((usize::from(st.work[177]) + usize::from(st.s(79))) >> 1) as u8,
            ((3 * usize::from(st.work[148])) / 5) as u8,
            st.matrix[1],
        ));
        st.matrix[16] = st.matrix[16]
            .add(((st.ma(4) & !0x60) | aux_hash | 8).sub(st.work[33].rotate_left(2) | 128));
        st.hash[14] ^= st.ma(2);
        st.work[19] = st.work[19].add(maj(
            rot(si(st.h(201)), (st.m(112) << 1) & 6),
            ((st.h(208) & !0x7c) | (st.h(164) & 0x7c)) / 5,
            37,
        ));
        st.matrix[8] = rot(140, neg(sq(st.s(45))) & 7) ^ st.aux[4];
        st.work[190] = 56;
        st.work[53] = !((st.h(83) | 204) / 5);
        st.hash[13] = st.hash[13].add(st.h(41));
        st.hash[10] = maj(st.ma(4), st.work[2], st.aux[2]) / 15;
        st.aux[3] = 92u8.sub(sq(0x28 | (st.ma(1) & (0x12 | (st.s(2) & 4)))));
        let seed_bits = si(st.aux[4]);
        st.matrix[13] ^= seed_bits;
        st.aux[6] = 92u8.add(sq(maj(st.m(179).sub(38), st.aux[2], 177)));
        let expansion_bits = maj(st.aux[3].add(st.aux[4] & 74), !seed_bits, 121);
        st.work[47] ^= st.m(89).add(maj(expansion_bits ^ 0xa6, st.aux[4], 4));
        st.aux[7] = (seed_bits / 3)
            .sub(st.ma(9))
            .sub(0x14 | (st.work[151] & ((st.aux[4] & 0x88) | 0x62)) | (st.aux[4] & 0x22));
        let expanded_selector = expansion_bits ^ ((st.aux[4] & 0xca) >> 1) ^ 75;
        st.aux[9] = st.aux[9].add(
            0x80 | (maj(st.aux[7], st.work[151], 0x20) & 0x64)
                | (seed_bits & 0x44)
                | (st.ma(9) & 0x1b),
        );
        st.matrix[33] ^= st.work[26];
        st.matrix[30] = ((st.aux[9] / 3).sub((st.aux[4] & !8) | 0x13)) ^ st.h(122);
        st.work[22] = (st.m(90) & 0x1b) | 0x44;
        let wide = usize::from(sel(
            71,
            st.matrix[usize::from(expanded_selector % 35)],
            si(st.aux[5]),
        ));
        st.matrix[18] = st.matrix[18].add(((wide * wide * wide) >> 1) as u8);
        st.matrix[5] = st.matrix[5].sub(st.s(92));
        st.matrix[18] ^= sel(st.aux[3], st.ma(3), sel(16, st.m(183), st.work[41])).mul(sel(
            expanded_selector,
            st.h(59),
            st.work[17],
        ));
        st.matrix[22] = maj(
            sel(st.hash[14] | 28, (st.work[7] & 28) | 0x82, st.h(93)),
            rot(st.ma(4), rot(st.work[11], neg(st.m(28)) & 7) & 7),
            st.matrix[33],
        )
        .add(74);
        st.hash[15] = st.hash[15].sub(maj(
            maj(st.aux[3], st.aux[4], 214),
            si(st.h(39) ^ 217),
            st.aux[6],
        ));

        let hash9 = st.hi(st.aux[9]);
        let indexed_hash = st.hi(((st.aux[4] / 3).sub(st.aux[9] | st.work[22]))
            ^ st.aux[6]
            ^ (((st.m(57) | hash9) & (0x52 | (st.aux[9] & 0x0d)))
                | (((st.m(57) & hash9) | st.aux[9]) & 0x20)));
        st.aux[6] = sq(sq(st.h(99))) | st.ma(9);
        st.aux[1] = st.aux[1]
            .add(rot(st.h(151) | st.s(202), st.h(50) & 7))
            .add(maj(
                st.h(4),
                ((usize::from(sel(st.matrix[16], indexed_hash, st.m(138)))
                    + usize::from(sel(17, st.work[33], st.s(39))))
                    / 5) as u8,
                147,
            ));
        st.aux[0] = sel(
            st.hash[10] & 7,
            st.ma(6) & st.h(209),
            sel(0x47, rot(st.s(127), st.ma(6) & 7), si(st.ma(5)) << 1),
        );
        let selected_square = sel(198, sq(st.m(14)), st.h(145) ^ st.aux[0]);
        let seed9 = si(st.aux[9]);
        let hash3 = st.hi(st.aux[3]);
        st.matrix[2] = st.matrix[2]
            .add(((hash3 << 1) & ((st.work[25] & 0x96) | (seed9 & 8))) | (seed9 & 0x40));
        st.matrix[14] = st.matrix[14].sub(sel(34, st.work[97], st.ma(3) & (st.aux[0] ^ st.m(100))));
        st.work[23] ^= maj(maj(st.s(17), hash3, st.aux[0]), st.work[50] / 3, 0x76) << 1;
        st.hash[17] = 115;
        st.hash[13] = ((maj(st.hi(st.aux[7]), st.work[10], 82) >> 1) & 0x68) | (st.h(39) & 0x17);
        st.matrix[33] = st.matrix[33].sub(st.work[113] & 9);
        st.matrix[28] = st.matrix[28].sub((st.aux[3] & !0x20) | ((st.work[110] >> 1) & 0x20));
        st.work[95] = si(st.aux[3]);
        st.hash[15] = maj(st.work[95].sub(48), !st.work[184], 189)
            & cube(maj(st.aux[7], si(st.aux[1]), 0xaa));
        st.matrix[22] = st.matrix[22].add(st.work[183]);
        st.aux[4] ^= 3u8.mul(st.s(1));
        st.aux[5] = st.aux[5].add(
            198u8
                .mul(maj(st.s(178), st.ma(1), 209))
                .mul(st.h(13))
                .mul(st.s(26) >> 1),
        );
        st.aux[8] = sel(10, st.ma(3), st.ma(9));
        st.matrix[18] = st.matrix[18].sub(sel(
            st.hash[15],
            st.aux[5] / 15,
            cube(st.hi(st.aux[6]) | 81),
        ));
        st.aux[1] = st.aux[1].add((si(st.hi(st.aux[1])) / 3).sub(st.h(160)));
        st.hash[16] = 147u8.sub(maj(
            st.aux[0],
            maj(
                st.s(69),
                st.work[172],
                st.aux[2].sub(selected_square).add(77),
            ),
            0xc2 | (st.aux[0] & 5),
        ));
        st.hash[3] = st.hash[3].sub(wide_seed(
            maj(st.s(155), st.work[105], 141),
            maj(st.s(168), st.h(29), 6) & 7,
        ));
        st.work[5] = rot(0x38, neg(st.h(61) / 5) & 7) ^ ((!st.ma(8)) / 5);
        st.work[198] = st.work[198].add(st.work[3]);
        let wide = usize::from(162 | st.ma(9));
        st.work[164] = st.work[164].add(((wide * wide) / 5) as u8);
        st.aux[2] = maj(rot(139, neg(st.aux[5]) & 6), st.hi(st.aux[3]), 12)
            | sel(95, cube(seed9), st.hi(st.aux[7]));
        st.matrix[12] =
            st.matrix[12].add((16 | ((st.work[103] | 60) & (st.aux[2] | (st.work[103] & 32)))) / 3);
        st.work[143] = st.work[143].sub(
            0x12 | (sel(
                st.aux[9],
                sel(st.matrix[8], st.work[35], st.aux[7]),
                st.aux[8] / 3,
            ) & (0x4d | ((st.work[172] >> 1) & 0x20))),
        );
        st.matrix[29] = 162;
        st.hash[15] = st.hash[15].add(maj(
            st.m(149) ^ sq(st.work[43]),
            sel(95, st.h(125), si(st.aux[1])) >> 1,
            115,
        ));
        st.aux[9] = st.aux[9].sub(st.hi(st.aux[7]));
        st.hash[7] = st.hash[7].sub(sq(rot(st.ma(5), neg(st.m(17)).mul(st.m(17) & 1))));
        st.matrix[8] = st.matrix[8].add(cube(st.s(202)).sub(st.work[184]));
        st.hash[16] = (st.m(102) << 1) & 0x84;
        st.aux[6] ^= si(st.aux[7]) >> 1;
        st.hash[7] = st.hash[7].sub(st.h(191).sub(sel(177, si(si(st.aux[1])), st.s(80) << 1)));
        st.hash[6] = st.h(119);
        st.hash[12] = (st.hi(st.aux[8]) ^ st.m(71).add(st.m(15)))
            & maj((st.work[118] & !0x2c) | 2, sq(st.hi(st.aux[9])), 27);
        let digest_index = sel(0xa9, st.s(57).mul(231), maj(st.work[32], st.ma(1), 23)) / 5;
        let seed_sample = si(st.aux[6]);
        st.aux[5] = maj(
            (seed_sample & 0x1c) | (st.h(82) & 0xa2) | (si(digest_index) & 0x41),
            maj(cube(st.hi(st.aux[7])), st.work[82], 92),
            192,
        ) ^ digest_index;
        st.matrix[25] ^= 2u8
            .mul(st.hi(st.aux[9]))
            .mul(st.work[5])
            .sub(rot(st.aux[4], seed_sample & 7) & st.aux[3].add(110));
    }
}

// ---- FairPlay message cipher ----

fn inverse_shift_rows(state: &mut [u8; 16]) {
    let prev = *state;
    for row in 0..4 {
        for col in 0..4 {
            state[4 * col + row] = prev[4 * ((col + 4 - row) & 3) + row];
        }
    }
}

fn shift_rows(state: &mut [u8; 16]) {
    let prev = *state;
    for row in 0..4 {
        for col in 0..4 {
            state[4 * col + row] = prev[4 * ((col + row) & 3) + row];
        }
    }
}

fn gf_multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        let high = a & 0x80;
        a <<= 1;
        if high != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    product
}

fn inverse_mix_columns(state: &mut [u8; 16]) {
    for col in 0..4 {
        let o = col * 4;
        let [a, b, c, d] = [state[o], state[o + 1], state[o + 2], state[o + 3]];
        state[o] = gf_multiply(a, 14) ^ gf_multiply(b, 11) ^ gf_multiply(c, 13) ^ gf_multiply(d, 9);
        state[o + 1] =
            gf_multiply(a, 9) ^ gf_multiply(b, 14) ^ gf_multiply(c, 11) ^ gf_multiply(d, 13);
        state[o + 2] =
            gf_multiply(a, 13) ^ gf_multiply(b, 9) ^ gf_multiply(c, 14) ^ gf_multiply(d, 11);
        state[o + 3] =
            gf_multiply(a, 11) ^ gf_multiply(b, 13) ^ gf_multiply(c, 9) ^ gf_multiply(d, 14);
    }
}

fn mix_columns(state: &mut [u8; 16]) {
    for col in 0..4 {
        let o = col * 4;
        let [a, b, c, d] = [state[o], state[o + 1], state[o + 2], state[o + 3]];
        state[o] = gf_multiply(a, 2) ^ gf_multiply(b, 3) ^ c ^ d;
        state[o + 1] = a ^ gf_multiply(b, 2) ^ gf_multiply(c, 3) ^ d;
        state[o + 2] = a ^ b ^ gf_multiply(c, 2) ^ gf_multiply(d, 3);
        state[o + 3] = gf_multiply(a, 3) ^ b ^ c ^ gf_multiply(d, 2);
    }
}

const fn forward_sbox() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        table[INVERSE_SBOX[i] as usize] = i as u8;
        i += 1;
    }
    table
}

const FORWARD_SBOX: [u8; 256] = forward_sbox();

fn xor_key(state: &mut [u8; 16], key: &[u8; 16]) {
    for (s, k) in state.iter_mut().zip(key) {
        *s ^= k;
    }
}

fn decrypt_block(state: &mut [u8; 16], mode: usize) {
    xor_key(state, &ROUND_KEY_10);
    for round in (1..=9).rev() {
        inverse_shift_rows(state);
        for b in state.iter_mut() {
            *b = INVERSE_SBOX[usize::from(*b)];
        }
        xor_key(state, &MIDDLE_KEYS[mode][round - 1]);
        inverse_mix_columns(state);
    }
    inverse_shift_rows(state);
    for b in state.iter_mut() {
        *b = INVERSE_SBOX[usize::from(*b)];
    }
    xor_key(state, &ROUND_KEY_0);
}

fn encrypt_block(state: &mut [u8; 16], mode: usize) {
    xor_key(state, &ROUND_KEY_0);
    for b in state.iter_mut() {
        *b = FORWARD_SBOX[usize::from(*b)];
    }
    shift_rows(state);
    for key in &MIDDLE_KEYS[mode] {
        mix_columns(state);
        xor_key(state, key);
        for b in state.iter_mut() {
            *b = FORWARD_SBOX[usize::from(*b)];
        }
        shift_rows(state);
    }
    xor_key(state, &ROUND_KEY_10);
}

fn check_mode(mode: u8) -> Result<usize> {
    let mode = usize::from(mode);
    if mode >= MESSAGE_IV.len() {
        return Err(Error::Crypto(format!(
            "modo de FairPlay {mode} no soportado"
        )));
    }
    Ok(mode)
}

// Decrypts the 128-byte body of an FPLY message (CBC with the per-mode IV).
pub(crate) fn decrypt_message(mode: u8, body: &[u8]) -> Result<[u8; 128]> {
    let mode = check_mode(mode)?;
    if body.len() != 128 {
        return Err(Error::Crypto(
            "cuerpo FairPlay de longitud no válida".into(),
        ));
    }
    let mut out = [0u8; 128];
    for block in 0..8 {
        let mut state = [0u8; 16];
        state.copy_from_slice(&body[block * 16..block * 16 + 16]);
        decrypt_block(&mut state, mode);
        let chain: &[u8] = if block == 0 {
            &MESSAGE_IV[mode]
        } else {
            &body[block * 16 - 16..block * 16]
        };
        for i in 0..16 {
            out[block * 16 + i] = state[i] ^ chain[i];
        }
    }
    Ok(out)
}

pub(crate) fn encrypt_message(mode: u8, plaintext: &[u8; 128]) -> Result<[u8; 128]> {
    let mode = check_mode(mode)?;
    let mut out = [0u8; 128];
    let mut chain = MESSAGE_IV[mode];
    for block in 0..8 {
        let mut state = [0u8; 16];
        for i in 0..16 {
            state[i] = plaintext[block * 16 + i] ^ chain[i];
        }
        encrypt_block(&mut state, mode);
        out[block * 16..block * 16 + 16].copy_from_slice(&state);
        chain = state;
    }
    Ok(out)
}

// ---- key derivation and wrapping ----

fn pad_message(parts: &[&[u8]]) -> [u8; 320] {
    let mut padded = [0u8; 320];
    let mut offset = 0;
    for part in parts {
        padded[offset..offset + part.len()].copy_from_slice(part);
        offset += part.len();
    }
    padded[offset] = 0x80;
    let bits = (offset as u64) * 8;
    padded[312..].copy_from_slice(&bits.to_le_bytes());
    padded
}

pub(crate) fn derive_wrapping_key(receiver_sap: &[u8; 128], m3: &[u8]) -> Result<[u8; 16]> {
    let decrypted = decrypt_message(m3[12], &m3[16..144])?;
    let material = pad_message(&[&KDF_PREFIX, &decrypted, receiver_sap, &KDF_SUFFIX]);
    let mut state = words_le(&INITIAL_SESSION_KEY);
    for block in material.chunks_exact(64) {
        let modified = md5_compress(state, block, Mutation::Kdf);
        let hashed = sap_hash(block);
        for (i, word) in state.iter_mut().enumerate() {
            let h = u32::from_le_bytes([
                hashed[i * 4],
                hashed[i * 4 + 1],
                hashed[i * 4 + 2],
                hashed[i * 4 + 3],
            ]);
            *word = modified[i].wrapping_add(h);
        }
    }
    Ok(words_be(state))
}

pub(crate) fn descriptor_for_sap(m3_sap: &[u8; 128], m2_sap: &[u8; 128]) -> [u8; 20] {
    let padded = pad_message(&[&DESCRIPTOR_PREFIX, m3_sap, m2_sap, &DESCRIPTOR_SUFFIX]);
    let mut state = words_le(&INITIAL_SESSION_KEY);
    let mut first_final = [0u32; 4];
    for (n, block) in padded.chunks_exact(64).enumerate() {
        let add = sap_hash(block);
        for (i, word) in state.iter_mut().enumerate() {
            let a =
                u32::from_le_bytes([add[i * 4], add[i * 4 + 1], add[i * 4 + 2], add[i * 4 + 3]]);
            *word = word.wrapping_add(a);
        }
        state = md5_compress(state, block, Mutation::Cycle);
        if n == padded.len() / 64 - 1 {
            first_final = state;
            state = md5_compress(state, block, Mutation::Cycle);
        }
    }
    let mut out = [0u8; 20];
    out[..4].copy_from_slice(&first_final[0].to_be_bytes());
    out[4..].copy_from_slice(&words_be(state));
    out
}

fn masks_for_seed(seed: &[u8; 20]) -> [[u8; 16]; 9] {
    let state = [0x1d4a4587, 0x92f39fcc, 0x1d87d836, 0xcdc86697];
    let suffix = [
        0x57, 0xd8, 0xee, 0xcb, 0xde, 0xfb, 0xcf, 0x59, 0x1c, 0x27, 0xa2, 0xcf, 0xbe, 0xb0, 0x89,
    ];
    std::array::from_fn(|i| {
        let mut block = [0u8; 64];
        block[..20].copy_from_slice(seed);
        block[20] = i as u8;
        block[21..36].copy_from_slice(&suffix);
        block[36] = 0x80;
        block[56..60].copy_from_slice(&0x320u32.to_le_bytes());
        words_be(md5_compress(state, &block, Mutation::Swap))
    })
}

fn digest32(left: &[u8; 16], right: &[u8; 16]) -> [u8; 16] {
    let mut block = [0u8; 64];
    block[..16].copy_from_slice(left);
    block[16..32].copy_from_slice(right);
    block[32] = 0x80;
    block[56..60].copy_from_slice(&0x100u32.to_le_bytes());
    let state = [0xb9f3dcdc, 0xfbdc740b, 0x60f77f86, 0x51907216];
    words_be(md5_compress(state, &block, Mutation::Swap))
}

fn mix_state(tables: &NetworkTables, state: &mut [u8; 16], substituted: &[u8; 16]) {
    for word in 0..4 {
        let offset = word * 4;
        for out_byte in 0..4 {
            let mut mixed = 0;
            for in_byte in 0..4 {
                mixed ^= mix(
                    tables.mix_columns[in_byte][out_byte],
                    substituted[offset + in_byte],
                );
            }
            state[offset + out_byte] = mixed;
        }
    }
}

fn first_network(masks: &[[u8; 16]; 9]) -> [u8; 16] {
    let mut state = FIXED_BLOCK;
    for (s, m) in state.iter_mut().zip(&FIRST_INPUT_MASK) {
        *s ^= m;
    }
    for (bank, mask) in masks.iter().enumerate() {
        let mut substituted = [0u8; 16];
        for (output, &input) in FIRST_POSITION_MAP.iter().enumerate() {
            substituted[output] =
                substitute(FIRST_TABLES.round_substitution[bank][input])(state[input]);
        }
        mix_state(&FIRST_TABLES, &mut state, &substituted);
        xor_key(&mut state, mask);
    }
    let mut out = [0u8; 16];
    for (output, &input) in FIRST_POSITION_MAP.iter().enumerate() {
        out[output] = substitute(FIRST_TABLES.final_substitution[input])(state[input]);
    }
    out
}

fn second_network(mut state: [u8; 16], masks: &[[u8; 16]; 9]) -> [u8; 16] {
    for bank in (0..9).rev() {
        let mut substituted = [0u8; 16];
        for (output, &input) in SECOND_POSITION_MAP.iter().enumerate() {
            substituted[output] =
                substitute(SECOND_TABLES.round_substitution[bank][output])(state[input])
                    ^ masks[bank][output];
        }
        mix_state(&SECOND_TABLES, &mut state, &substituted);
    }
    let mut out = [0u8; 16];
    for (output, &input) in SECOND_POSITION_MAP.iter().enumerate() {
        out[output] = substitute(SECOND_TABLES.final_substitution[output])(state[input])
            ^ SECOND_OUTPUT_MASK[output];
    }
    out
}

fn exchange_seed(seed: &[u8; 20]) -> [u8; 20] {
    let masks = masks_for_seed(seed);
    let intermediate = first_network(&masks);
    let left = digest32(&intermediate, &FIXED_BLOCK);
    let whitebox = second_network(left, &masks);
    let digest = digest32(&left, &whitebox);
    let mut out = [0u8; 20];
    out[..4].copy_from_slice(&whitebox[..4]);
    out[4..].copy_from_slice(&digest);
    out
}

pub(crate) fn exchange_for_sap(m3_sap: &[u8; 128], m2_sap: &[u8; 128]) -> [u8; 20] {
    exchange_seed(&descriptor_for_sap(m3_sap, m2_sap))
}

fn sha1_hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    use sha1::Digest;
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha1::new();
    inner.update(k.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha1::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn record(message_type: u8, payload_length: usize) -> Vec<u8> {
    let mut r = vec![0u8; 12 + payload_length];
    r[..4].copy_from_slice(b"FPLY");
    r[4..8].copy_from_slice(&[3, 1, message_type, 0]);
    r[8..12].copy_from_slice(&(payload_length as u32).to_be_bytes());
    r
}

fn validate_record(rec: &[u8], message_type: u8, payload_length: usize) -> Result<()> {
    if rec.len() != 12 + payload_length {
        return Err(Error::protocol(format!(
            "longitud FairPlay {}, se esperaba {}",
            rec.len(),
            12 + payload_length
        )));
    }
    if &rec[..4] != b"FPLY" {
        return Err(Error::protocol("firma FairPlay no válida"));
    }
    if rec[4] != 3 || rec[5] != 1 || rec[6] != message_type || rec[7] != 0 {
        return Err(Error::protocol("versión o tipo FairPlay no válidos"));
    }
    if u32::from_be_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize != payload_length {
        return Err(Error::protocol("longitud declarada FairPlay no válida"));
    }
    Ok(())
}

pub(crate) fn wrap_with(
    receiver_sap: &[u8; 128],
    m3: &[u8],
    raw_key: &[u8; 16],
    mask: [u8; 16],
) -> Result<[u8; 72]> {
    validate_record(m3, 3, 152).map_err(|_| Error::Crypto("m3 de FairPlay no válido".into()))?;
    check_mode(m3[12])?;
    let mut ekey = [0u8; 72];
    ekey[..16].copy_from_slice(&[
        b'F', b'P', b'L', b'Y', 0x01, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x00, 0x00,
        0x00,
    ]);
    ekey[16..32].copy_from_slice(&mask);
    ekey[32..36].copy_from_slice(&16u32.to_be_bytes());

    let wrapping_key = derive_wrapping_key(receiver_sap, m3)?;
    let cipher = Aes128::new(&Array::from(wrapping_key));
    let mut masked = Array::from(std::array::from_fn::<u8, 16, _>(|i| raw_key[i] ^ mask[i]));
    cipher.encrypt_block(&mut masked);
    ekey[56..72].copy_from_slice(&masked);

    let sender_sap = decrypt_message(m3[12], &m3[16..144])?;
    let mac_key = descriptor_for_sap(&sender_sap, receiver_sap);
    let mac = sha1_hmac(&mac_key, &[&ekey[..36], raw_key]);
    ekey[36..56].copy_from_slice(&mac);
    Ok(ekey)
}

pub struct FairPlayClient {
    local_sap: [u8; 128],
    remote_sap: [u8; 128],
    m3: [u8; 164],
    has_m3: bool,
}

impl Default for FairPlayClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FairPlayClient {
    pub fn new() -> Self {
        let mut entropy = [0u8; 126];
        rand::fill(&mut entropy[..]);
        Self::with_entropy(entropy)
    }

    pub(crate) fn with_entropy(entropy: [u8; 126]) -> Self {
        let mut local_sap = [0u8; 128];
        local_sap[1] = 1;
        local_sap[2..].copy_from_slice(&entropy);
        Self {
            local_sap,
            remote_sap: [0; 128],
            m3: [0; 164],
            has_m3: false,
        }
    }

    pub fn message1(&self) -> Vec<u8> {
        let mut m1 = record(1, M1_PAYLOAD.len());
        m1[12..].copy_from_slice(&M1_PAYLOAD);
        m1
    }

    pub fn exchange(&mut self, m2: &[u8]) -> Result<Vec<u8>> {
        validate_record(m2, 2, 130)?;
        if m2[12] != 2 {
            return Err(Error::protocol("marcador de carga FairPlay m2 no válido"));
        }
        let mode = m2[13];
        check_mode(mode)?;

        let mut m3 = record(3, 152);
        m3[12] = mode;
        m3[13..16].copy_from_slice(&M3_LABEL);
        m3[16..144].copy_from_slice(&encrypt_message(mode, &self.local_sap)?);

        let remote_sap = decrypt_message(mode, &m2[14..142])?;
        let tail = exchange_for_sap(&self.local_sap, &remote_sap);
        m3[144..].copy_from_slice(&tail);
        self.remote_sap = remote_sap;
        self.m3.copy_from_slice(&m3);
        self.has_m3 = true;
        Ok(m3)
    }

    pub fn confirm(&mut self, m4: &[u8]) -> Result<()> {
        if !self.has_m3 {
            return Err(Error::protocol("m3 de FairPlay aún no generado"));
        }
        validate_record(m4, 4, 20)?;
        if m4[12..] != self.m3[144..] {
            return Err(Error::protocol(
                "la confirmación m4 de FairPlay no coincide",
            ));
        }
        Ok(())
    }

    pub fn wrap_key(&self, raw_key: &[u8; 16]) -> Result<[u8; 72]> {
        let mut mask = [0u8; 16];
        rand::fill(&mut mask[..]);
        self.wrap_key_with_mask(raw_key, mask)
    }

    pub(crate) fn wrap_key_with_mask(
        &self,
        raw_key: &[u8; 16],
        mask: [u8; 16],
    ) -> Result<[u8; 72]> {
        if !self.has_m3 {
            return Err(Error::protocol("m3 de FairPlay aún no generado"));
        }
        wrap_with(&self.remote_sap, &self.m3, raw_key, mask)
    }

    #[cfg(test)]
    pub(crate) fn remote_sap(&self) -> [u8; 128] {
        self.remote_sap
    }
}

#[cfg(test)]
mod tests {
    use aes::cipher::BlockCipherDecrypt;
    use sha2::{Digest, Sha256};

    use super::super::sap_vectors::*;
    use super::*;

    pub fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn arr128(s: &str) -> [u8; 128] {
        unhex(s).try_into().unwrap()
    }

    fn unwrap_key(receiver_sap: &[u8; 128], m3: &[u8], ekey: &[u8]) -> [u8; 16] {
        let key = derive_wrapping_key(receiver_sap, m3).unwrap();
        let cipher = Aes128::new(&Array::from(key));
        let mut block = Array::from(<[u8; 16]>::try_from(&ekey[56..72]).unwrap());
        cipher.decrypt_block(&mut block);
        std::array::from_fn(|i| block[i] ^ ekey[16 + i])
    }

    fn payload_with(index: Option<usize>, fill: u8, value: u8) -> [u8; 128] {
        let mut p = [fill; 128];
        if let Some(i) = index {
            p[i] = value;
        }
        p
    }

    fn decrypted_body(payload: &[u8; 128]) -> [u8; 128] {
        decrypt_message(3, payload).unwrap()
    }

    #[test]
    fn table_data_checksum() {
        let mut hash = Sha256::new();
        let mut written = 0;
        hash.update(FIRST_INPUT_MASK);
        written += 16;
        for tables in [&FIRST_TABLES, &SECOND_TABLES] {
            for round in &tables.round_substitution {
                for &l in round {
                    let f = substitute(l);
                    let expanded: Vec<u8> = (0..=255u8).map(&f).collect();
                    hash.update(&expanded);
                    written += expanded.len();
                }
            }
            for input_table in &tables.mix_columns {
                let mut expanded = Vec::with_capacity(1024);
                for value in 0..=255u8 {
                    for &l in input_table {
                        expanded.push(mix(l, value));
                    }
                }
                hash.update(&expanded);
                written += expanded.len();
            }
            for &l in &tables.final_substitution {
                let f = substitute(l);
                let expanded: Vec<u8> = (0..=255u8).map(&f).collect();
                hash.update(&expanded);
                written += expanded.len();
            }
        }
        assert_eq!(written, 90128);
        assert_eq!(
            hex(&hash.finalize()),
            "28d0986abebe30458348dfa2957aa1d52d6f3ad5a9468c5d8a9c4139b7ca2b43"
        );
    }

    #[test]
    fn primitive_vectors() {
        let block: [u8; 64] = std::array::from_fn(|i| (i * 3 + 1) as u8);
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let words = md5_compress(words_le(&key), &block, Mutation::Kdf);
        let modified: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(hex(&modified), "f6f728cb5a4397b675664f9291b859aa");
        assert_eq!(hex(&sap_hash(&block)), "75498a4e218773030e9cdf04f0c49367");
    }

    #[test]
    fn sap_hash_corpus() {
        let mut corpus = Sha256::new();
        let mut state: u64 = 0x6a09e667f3bcc909;
        for _ in 0..64 {
            let mut block = [0u8; 64];
            for b in block.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = state as u8;
            }
            corpus.update(sap_hash(&block));
        }
        assert_eq!(
            hex(&corpus.finalize()),
            "36ad2a7920076af59452d9f0c91e3b7d1aebc53f9143bd6819e39119d4535c92"
        );
    }

    #[test]
    fn message_vectors() {
        for (mode, decrypted, aes_key) in MESSAGE_VECTORS {
            let mut message = [0u8; 164];
            message[12] = mode;
            for (i, b) in message.iter_mut().enumerate().take(144).skip(16) {
                *b = (i * 5 + 7) as u8;
            }
            let plain = decrypt_message(mode, &message[16..144]).unwrap();
            assert_eq!(hex(&plain), decrypted);
            let enc = encrypt_message(mode, &plain).unwrap();
            assert_eq!(enc[..], message[16..144]);
            let key = derive_wrapping_key(&arr128(REF_RECEIVER_SAP), &message).unwrap();
            assert_eq!(hex(&key), aes_key);
        }
    }

    #[test]
    fn key_unwrap_vector() {
        let mut m3 = [0u8; 164];
        m3[12] = 3;
        for (i, b) in m3.iter_mut().enumerate().take(144).skip(16) {
            *b = (i * 5 + 7) as u8;
        }
        let ekey: Vec<u8> = (0..72).map(|i| (i * 7 + 3) as u8).collect();
        let got = unwrap_key(&arr128(REF_RECEIVER_SAP), &m3, &ekey);
        assert_eq!(hex(&got), "903e5be94732428e9965afb262b193a4");
    }

    #[test]
    fn exchange_golden_vectors() {
        let captured = unhex(CAPTURED_M2);
        let local = arr128(LOCAL_SAP);
        let mut captured_payload = [0u8; 128];
        captured_payload.copy_from_slice(&captured[14..142]);
        let cases: [([u8; 128], &str); 7] = [
            (
                payload_with(None, 0, 0),
                "6f627565f3e77f5b5ede91beee7baf92e4241e0b",
            ),
            (
                payload_with(None, 0xff, 0),
                "dc2cc74f2ed55484f59f95b96082f0f5c017dd17",
            ),
            (captured_payload, "4b911e48af23d8406368aeafbb61bfcd569e3e55"),
            (
                payload_with(Some(0), 0, 0x42),
                "9bfb9556b8659c2ac94b7ef9e587d71e159ea624",
            ),
            (
                payload_with(Some(63), 0, 0x42),
                "150d9fa4eb456e73ba48de5779c5c996b16b3b23",
            ),
            (
                payload_with(Some(64), 0, 0x42),
                "a167db30424ff8890d085c0f1c92b2c5cc06fc45",
            ),
            (
                payload_with(Some(127), 0, 0x42),
                "d246ec5e7adc8118994b8df77146529486ac7caf",
            ),
        ];
        for (payload, want) in cases {
            let got = exchange_for_sap(&local, &decrypted_body(&payload));
            assert_eq!(hex(&got), want);
        }
    }

    #[test]
    fn descriptor_vectors() {
        let local = arr128(LOCAL_SAP);
        let zero = decrypted_body(&[0; 128]);
        assert_eq!(
            hex(&descriptor_for_sap(&local, &zero)),
            "7e38958ffe4ed433743919fe7eb16376afa4eb9e"
        );
        let one = decrypted_body(&payload_with(Some(0), 0, 1));
        assert_eq!(
            hex(&descriptor_for_sap(&local, &one)),
            "ea46797d726c6a9be43ffa72385ff97ce1c54f1b"
        );
    }

    fn test_m3(mode: u8) -> Vec<u8> {
        let mut m3 = record(3, 152);
        m3[12] = mode;
        for (i, b) in m3.iter_mut().enumerate().skip(13) {
            *b = (i * 7 + 3) as u8;
        }
        m3
    }

    fn test_receiver_sap(seed: u8) -> [u8; 128] {
        let mut sap = [0u8; 128];
        sap[1] = 1;
        for (i, b) in sap.iter_mut().enumerate().skip(2) {
            *b = ((i * 11) as u8) ^ seed;
        }
        sap
    }

    #[test]
    fn wrap_round_trip_for_every_mode() {
        for mode in 0..4u8 {
            let m3 = test_m3(mode);
            let receiver_sap = test_receiver_sap(mode);
            let raw: [u8; 16] = std::array::from_fn(|i| (0x10 + 0x11 * i) as u8 ^ mode);
            let mask: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
            let ekey = wrap_with(&receiver_sap, &m3, &raw, mask).unwrap();
            assert_eq!(hex(&ekey[..16]), "46504c59010201000000003c00000000");
            assert_eq!(ekey[16..32], mask);
            assert_eq!(u32::from_be_bytes(ekey[32..36].try_into().unwrap()), 16);
            let sender_sap = decrypt_message(mode, &m3[16..144]).unwrap();
            let mac_key = descriptor_for_sap(&sender_sap, &receiver_sap);
            assert_eq!(ekey[36..56], sha1_hmac(&mac_key, &[&ekey[..36], &raw]));
            assert_eq!(unwrap_key(&receiver_sap, &m3, &ekey), raw);
        }
    }

    #[test]
    fn wrap_rejects_invalid_input() {
        let sap = test_receiver_sap(3);
        assert!(wrap_with(&sap, &[0u8; 143], &[0; 16], [0; 16]).is_err());
        let mut m3 = test_m3(3);
        m3[12] = 4;
        assert!(wrap_with(&sap, &m3, &[0; 16], [0; 16]).is_err());
    }

    #[test]
    fn alternate_mac_vector() {
        let m2 = unhex(ALT_M2);
        let m3 = unhex(ALT_M3);
        let raw: [u8; 16] = std::array::from_fn(|i| i as u8);
        let mask: [u8; 16] = unhex("c853e777b9b65e7652d768d97c974f15")
            .try_into()
            .unwrap();
        let receiver_sap = decrypt_message(m2[13], &m2[14..142]).unwrap();
        assert_eq!(receiver_sap, arr128(REF_RECEIVER_SAP));
        let ekey = wrap_with(&receiver_sap, &m3, &raw, mask).unwrap();
        assert_eq!(
            hex(&ekey[..56]),
            "46504c59010201000000003c00000000c853e777b9b65e7652d768d97c974f15000000102a53a0008888fe26bfb1e1f825f38f50d6730059"
        );
        let sender_sap = decrypt_message(m3[12], &m3[16..144]).unwrap();
        assert_eq!(
            hex(&descriptor_for_sap(&sender_sap, &receiver_sap)),
            "2fd95dc2c23122bc77c57b983a9188c4760db322"
        );
    }

    #[test]
    fn key_decrypt_vector() {
        let m3 = unhex(DEC_M3);
        let ekey = unhex(DEC_EKEY);
        assert_eq!(u32::from_be_bytes(ekey[32..36].try_into().unwrap()), 16);
        let got = unwrap_key(&arr128(REF_RECEIVER_SAP), &m3, &ekey);
        assert_eq!(hex(&got), "8e1214398d46d72e7b1b8e32f80c8bf0");
    }

    fn fake_m2(mode: u8, body: &[u8; 128]) -> Vec<u8> {
        let mut m2 = record(2, 130);
        m2[12] = 2;
        m2[13] = mode;
        m2[14..142].copy_from_slice(&encrypt_message(mode, body).unwrap());
        m2
    }

    #[test]
    fn session_exchange_for_each_mode() {
        let entropy: [u8; 126] = std::array::from_fn(|i| i as u8 + 1);
        let receiver_sap = test_receiver_sap(9);
        for mode in 0..4u8 {
            let mut client = FairPlayClient::with_entropy(entropy);
            assert_eq!(hex(&client.message1()), "46504c590301010000000004020003bb");
            let m3 = client.exchange(&fake_m2(mode, &receiver_sap)).unwrap();
            assert_eq!(m3.len(), 164);
            assert_eq!(m3[12], mode);
            let local_sap = decrypt_message(mode, &m3[16..144]).unwrap();
            let mut want = [0u8; 128];
            want[1] = 1;
            want[2..].copy_from_slice(&entropy);
            assert_eq!(local_sap, want);
            assert_eq!(client.remote_sap(), receiver_sap);
            assert_eq!(m3[144..], exchange_for_sap(&want, &receiver_sap));

            let raw: [u8; 16] = std::array::from_fn(|i| i as u8);
            let ekey = client.wrap_key_with_mask(&raw, [0x5a; 16]).unwrap();
            assert_eq!(unwrap_key(&receiver_sap, &m3, &ekey), raw);
        }
    }

    #[test]
    fn session_rejects_bad_input_and_order() {
        let entropy: [u8; 126] = std::array::from_fn(|i| i as u8 + 1);
        let sap = test_receiver_sap(1);
        let mut client = FairPlayClient::with_entropy(entropy);
        assert!(client.confirm(&[0; 32]).is_err());
        assert!(client.wrap_key(&[0; 16]).is_err());
        assert!(client.exchange(&[0; 141]).is_err());
        for mode in [4u8, 0xff] {
            let mut m2 = fake_m2(0, &sap);
            m2[13] = mode;
            assert!(client.exchange(&m2).is_err());
        }
        let m3 = client.exchange(&fake_m2(3, &sap)).unwrap();
        let mut other = FairPlayClient::with_entropy([0xa5; 126]);
        let other_m3 = other.exchange(&fake_m2(3, &sap)).unwrap();
        assert_ne!(m3[16..144], other_m3[16..144]);
    }

    #[test]
    fn m4_confirmation() {
        let entropy: [u8; 126] = std::array::from_fn(|i| i as u8 + 1);
        let mut client = FairPlayClient::with_entropy(entropy);
        let m3 = client.exchange(&fake_m2(3, &test_receiver_sap(1))).unwrap();
        let mut m4 = record(4, 20);
        m4[12..].copy_from_slice(&m3[144..]);
        client.confirm(&m4).unwrap();
        m4[31] ^= 1;
        assert!(client.confirm(&m4).is_err());
    }
}
