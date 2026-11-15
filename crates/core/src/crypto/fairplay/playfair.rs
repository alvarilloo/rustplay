// Port of UxPlay lib/playfair (omg_hax.c, modified_md5.c, sap_hash.c, playfair.c).
use super::garble::garble;
use super::tables::*;

type KeySchedule = [[u8; 16]; 11];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_SHIFT: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const INDEX_MANGLE: [u8; 11] = [
    0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36, 0x6c,
];

fn le_word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn xor16(block: &mut [u8; 16], key: &[u8; 16]) {
    for (b, k) in block.iter_mut().zip(key) {
        *b ^= k;
    }
}

fn modified_md5(block_in: &[u8; 64], key_in: &[u8; 16]) -> [u8; 16] {
    let mut w = [0u32; 16];
    for (j, word) in w.iter_mut().enumerate() {
        *word = u32::from_be_bytes([
            block_in[4 * j],
            block_in[4 * j + 1],
            block_in[4 * j + 2],
            block_in[4 * j + 3],
        ]);
    }
    let k0 = le_word(&key_in[0..4]);
    let k1 = le_word(&key_in[4..8]);
    let k2 = le_word(&key_in[8..12]);
    let k3 = le_word(&key_in[12..16]);
    let (mut a, mut b, mut c, mut d) = (k0, k1, k2, k3);
    for i in 0..64usize {
        let j = match i {
            0..16 => i,
            16..32 => (5 * i + 1) % 16,
            32..48 => (3 * i + 5) % 16,
            _ => 7 * i % 16,
        };
        let z = a.wrapping_add(w[j]).wrapping_add(MD5_K[i]);
        let f = match i {
            0..16 => (b & c) | (!b & d),
            16..32 => (b & d) | (c & !d),
            32..48 => b ^ c ^ d,
            _ => c ^ (b | !d),
        };
        let z = z.wrapping_add(f).rotate_left(MD5_SHIFT[i]).wrapping_add(b);
        let tmp = d;
        d = c;
        c = b;
        b = z;
        a = tmp;
        if i == 31 {
            w.swap((a & 15) as usize, (b & 15) as usize);
            w.swap((c & 15) as usize, (d & 15) as usize);
            w.swap(((a >> 4) & 15) as usize, ((b >> 4) & 15) as usize);
            w.swap(((a >> 8) & 15) as usize, ((b >> 8) & 15) as usize);
            w.swap(((a >> 12) & 15) as usize, ((b >> 12) & 15) as usize);
        }
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&k0.wrapping_add(a).to_le_bytes());
    out[4..8].copy_from_slice(&k1.wrapping_add(b).to_le_bytes());
    out[8..12].copy_from_slice(&k2.wrapping_add(c).to_le_bytes());
    out[12..16].copy_from_slice(&k3.wrapping_add(d).to_le_bytes());
    out
}

fn rol8(input: u8, count: u32) -> u8 {
    input.rotate_left(count)
}

fn sap_hash(block_in: &[u8; 64], key_out: &mut [u8; 16]) {
    let mut buffer0: [u8; 20] = [
        0x96, 0x5F, 0xC6, 0x53, 0xF8, 0x46, 0xCC, 0x18, 0xDF, 0xBE, 0xB2, 0xF8, 0x38, 0xD7, 0xEC,
        0x22, 0x03, 0xD1, 0x20, 0x8F,
    ];
    let mut buffer1 = [0u8; 210];
    let mut buffer2: [u8; 35] = [
        0x43, 0x54, 0x62, 0x7A, 0x18, 0xC3, 0xD6, 0xB3, 0x9A, 0x56, 0xF6, 0x1C, 0x14, 0x3F, 0x0C,
        0x1D, 0x3B, 0x36, 0x83, 0xB1, 0x39, 0x51, 0x4A, 0xAA, 0x09, 0x3E, 0xFE, 0x44, 0xAF, 0xDE,
        0xC3, 0x20, 0x9D, 0x42, 0x3A,
    ];
    let mut buffer3 = [0u8; 132];
    let buffer4: [u8; 21] = [
        0xED, 0x25, 0xD1, 0xBB, 0xBC, 0x27, 0x9F, 0x02, 0xA2, 0xA9, 0x11, 0x00, 0x0C, 0xB3, 0x52,
        0xC0, 0xBD, 0xE3, 0x1B, 0x49, 0xC7,
    ];
    const I0_INDEX: [usize; 11] = [18, 22, 23, 0, 5, 19, 32, 31, 10, 21, 30];

    for (i, out) in buffer1.iter_mut().enumerate() {
        let group = (i % 64) & !3;
        *out = block_in[group + 3 - (i % 4)];
    }
    for i in 0..840u32 {
        let x = buffer1[(i.wrapping_sub(155) % 210) as usize];
        let y = buffer1[(i.wrapping_sub(57) % 210) as usize];
        let z = buffer1[(i.wrapping_sub(13) % 210) as usize];
        let w = buffer1[(i % 210) as usize];
        buffer1[(i % 210) as usize] = rol8(y, 5)
            .wrapping_add(rol8(z, 3) ^ w)
            .wrapping_sub(rol8(x, 7));
    }
    garble(
        &mut buffer0,
        &mut buffer1,
        &mut buffer2,
        &mut buffer3,
        &buffer4,
    );

    *key_out = [0xe1; 16];
    for (i, &idx) in I0_INDEX.iter().enumerate() {
        if i == 3 {
            key_out[i] = 0x3d;
        } else {
            key_out[i] = key_out[i].wrapping_add(buffer3[idx * 4]);
        }
    }
    for (i, v) in buffer0.iter().enumerate() {
        key_out[i % 16] ^= v;
    }
    for (i, v) in buffer2.iter().enumerate() {
        key_out[i % 16] ^= v;
    }
    for (i, v) in buffer1.iter().enumerate() {
        key_out[i % 16] ^= v;
    }
    for _ in 0..16 {
        for i in 0..16usize {
            let x = key_out[(i + 16 - 7) % 16];
            let y = key_out[i];
            let z = key_out[(i + 16 - 37 % 16) % 16];
            let w = key_out[(i + 16 - 177 % 16) % 16];
            key_out[i] = rol8(x, 1) ^ y ^ rol8(z, 6) ^ rol8(w, 5);
        }
    }
}

fn table_index(i: usize) -> &'static [u8] {
    let start = ((31 * i) % 0x28) << 8;
    &TABLE_S1[start..start + 256]
}

fn generate_key_schedule(key_material: &[u8; 16]) -> KeySchedule {
    let mut schedule = [[0u8; 16]; 11];
    let mut buffer = *key_material;
    xor16(&mut buffer, &T_KEY);
    let mut ti = 0;
    for (round, entry) in schedule.iter_mut().enumerate() {
        entry[0..4].copy_from_slice(&buffer[0..4]);
        let t1 = table_index(ti);
        let t2 = table_index(ti + 1);
        let t3 = table_index(ti + 2);
        let t4 = table_index(ti + 3);
        ti += 4;
        buffer[0] ^= t1[buffer[0x0d] as usize] ^ INDEX_MANGLE[round];
        buffer[1] ^= t2[buffer[0x0e] as usize];
        buffer[2] ^= t3[buffer[0x0f] as usize];
        buffer[3] ^= t4[buffer[0x0c] as usize];
        entry[4..8].copy_from_slice(&buffer[4..8]);
        for k in 0..4 {
            buffer[4 + k] ^= buffer[k];
        }
        entry[8..12].copy_from_slice(&buffer[8..12]);
        for k in 0..4 {
            buffer[8 + k] ^= buffer[4 + k];
        }
        entry[12..16].copy_from_slice(&buffer[12..16]);
        for k in 0..4 {
            buffer[12 + k] ^= buffer[8 + k];
        }
    }
    schedule
}

fn permute_block_1(block: &mut [u8; 16]) {
    block[0] = TABLE_S3[block[0] as usize];
    block[4] = TABLE_S3[0x400 + block[4] as usize];
    block[8] = TABLE_S3[0x800 + block[8] as usize];
    block[12] = TABLE_S3[0xc00 + block[12] as usize];

    let tmp = block[13];
    block[13] = TABLE_S3[0x100 + block[9] as usize];
    block[9] = TABLE_S3[0xd00 + block[5] as usize];
    block[5] = TABLE_S3[0x900 + block[1] as usize];
    block[1] = TABLE_S3[0x500 + tmp as usize];

    let tmp = block[2];
    block[2] = TABLE_S3[0xa00 + block[10] as usize];
    block[10] = TABLE_S3[0x200 + tmp as usize];
    let tmp = block[6];
    block[6] = TABLE_S3[0xe00 + block[14] as usize];
    block[14] = TABLE_S3[0x600 + tmp as usize];

    let tmp = block[3];
    block[3] = TABLE_S3[0xf00 + block[7] as usize];
    block[7] = TABLE_S3[0x300 + block[11] as usize];
    block[11] = TABLE_S3[0x700 + block[15] as usize];
    block[15] = TABLE_S3[0xb00 + tmp as usize];
}

fn t2(i: usize, v: u8) -> u8 {
    TABLE_S4[(((71 * i) % 144) << 8) + v as usize]
}

fn permute_block_2(block: &mut [u8; 16], round: usize) {
    let r = round * 16;
    block[0] = t2(r, block[0]);
    block[4] = t2(r + 4, block[4]);
    block[8] = t2(r + 8, block[8]);
    block[12] = t2(r + 12, block[12]);

    let tmp = block[13];
    block[13] = t2(r + 13, block[9]);
    block[9] = t2(r + 9, block[5]);
    block[5] = t2(r + 5, block[1]);
    block[1] = t2(r + 1, tmp);

    let tmp = block[2];
    block[2] = t2(r + 2, block[10]);
    block[10] = t2(r + 10, tmp);
    let tmp = block[6];
    block[6] = t2(r + 6, block[14]);
    block[14] = t2(r + 14, tmp);

    let tmp = block[3];
    block[3] = t2(r + 3, block[7]);
    block[7] = t2(r + 7, block[11]);
    block[11] = t2(r + 11, block[15]);
    block[15] = t2(r + 15, tmp);
}

fn cycle(block: &mut [u8; 16], schedule: &KeySchedule) {
    xor16(block, &schedule[10]);
    permute_block_1(block);

    for round in 0..9usize {
        let k = &schedule[9 - round];
        let w0 = TABLE_S5[(block[3] ^ k[3]) as usize]
            ^ TABLE_S6[(block[2] ^ k[2]) as usize]
            ^ TABLE_S8[(block[0] ^ k[0]) as usize]
            ^ TABLE_S7[(block[1] ^ k[1]) as usize];
        block[0..4].copy_from_slice(&w0.to_le_bytes());
        let w1 = TABLE_S5[(block[7] ^ k[7]) as usize]
            ^ TABLE_S6[(block[6] ^ k[6]) as usize]
            ^ TABLE_S7[(block[5] ^ k[5]) as usize]
            ^ TABLE_S8[(block[4] ^ k[4]) as usize];
        block[4..8].copy_from_slice(&w1.to_le_bytes());
        let w2 = TABLE_S5[(block[11] ^ k[11]) as usize]
            ^ TABLE_S6[(block[10] ^ k[10]) as usize]
            ^ TABLE_S7[(block[9] ^ k[9]) as usize]
            ^ TABLE_S8[(block[8] ^ k[8]) as usize];
        block[8..12].copy_from_slice(&w2.to_le_bytes());
        let w3 = TABLE_S5[(block[15] ^ k[15]) as usize]
            ^ TABLE_S6[(block[14] ^ k[14]) as usize]
            ^ TABLE_S7[(block[13] ^ k[13]) as usize]
            ^ TABLE_S8[(block[12] ^ k[12]) as usize];
        block[12..16].copy_from_slice(&w3.to_le_bytes());
        permute_block_2(block, 8 - round);
    }
    xor16(block, &schedule[0]);
}

fn message_key(mode: usize) -> &'static [u8] {
    match mode {
        0 => &MESSAGE_KEY_0,
        1 => &MESSAGE_KEY_1,
        2 => &MESSAGE_KEY_2,
        _ => &MESSAGE_KEY_3,
    }
}

fn message_iv(mode: usize) -> &'static [u8; 16] {
    match mode {
        0 => &MESSAGE_IV_0,
        1 => &MESSAGE_IV_1,
        2 => &MESSAGE_IV_2,
        _ => &MESSAGE_IV_3,
    }
}

fn mt(i: usize, v: u8) -> u8 {
    TABLE_S2[(((97 * i) % 144) << 8) + v as usize]
}

fn s9_word(buffer: &[u8; 16], k: usize) -> u32 {
    TABLE_S9[buffer[k] as usize]
        ^ TABLE_S9[0x100 + buffer[k + 1] as usize]
        ^ TABLE_S9[0x200 + buffer[k + 2] as usize]
        ^ TABLE_S9[0x300 + buffer[k + 3] as usize]
}

fn decrypt_message(message_in: &[u8; 164], out: &mut [u8; 128]) {
    let mode = usize::from(message_in[12]) & 3;
    let mkey = message_key(mode);
    for i in 0..8usize {
        let mut buffer = [0u8; 16];
        let src = if mode == 3 {
            0x80 - 0x10 * i
        } else {
            0x10 * (i + 1)
        };
        buffer.copy_from_slice(&message_in[src..src + 16]);
        for j in 0..9usize {
            let base = 0x80 - 0x10 * j;
            let f = |k: usize, v: u8| mt(base + k, v) ^ mkey[base + k];
            buffer[0x0] = f(0x0, buffer[0x0]);
            buffer[0x4] = f(0x4, buffer[0x4]);
            buffer[0x8] = f(0x8, buffer[0x8]);
            buffer[0xc] = f(0xc, buffer[0xc]);

            let tmp = buffer[0x0d];
            buffer[0xd] = f(0xd, buffer[0x9]);
            buffer[0x9] = f(0x9, buffer[0x5]);
            buffer[0x5] = f(0x5, buffer[0x1]);
            buffer[0x1] = f(0x1, tmp);

            let tmp = buffer[0x02];
            buffer[0x2] = f(0x2, buffer[0xa]);
            buffer[0xa] = f(0xa, tmp);
            let tmp = buffer[0x06];
            buffer[0x6] = f(0x6, buffer[0xe]);
            buffer[0xe] = f(0xe, tmp);

            let tmp = buffer[0x3];
            buffer[0x3] = f(0x3, buffer[0x7]);
            buffer[0x7] = f(0x7, buffer[0xb]);
            buffer[0xb] = f(0xb, buffer[0xf]);
            buffer[0xf] = f(0xf, tmp);

            let w0 = s9_word(&buffer, 0);
            let w1 = s9_word(&buffer, 4);
            let w2 = s9_word(&buffer, 8);
            let w3 = s9_word(&buffer, 12);
            buffer[0..4].copy_from_slice(&w0.to_le_bytes());
            buffer[4..8].copy_from_slice(&w1.to_le_bytes());
            buffer[8..12].copy_from_slice(&w2.to_le_bytes());
            buffer[12..16].copy_from_slice(&w3.to_le_bytes());
        }
        let s10 = |k: usize, v: u8| TABLE_S10[(k << 8) + v as usize];
        buffer[0x0] = s10(0x0, buffer[0x0]);
        buffer[0x4] = s10(0x4, buffer[0x4]);
        buffer[0x8] = s10(0x8, buffer[0x8]);
        buffer[0xc] = s10(0xc, buffer[0xc]);

        let tmp = buffer[0x0d];
        buffer[0xd] = s10(0xd, buffer[0x9]);
        buffer[0x9] = s10(0x9, buffer[0x5]);
        buffer[0x5] = s10(0x5, buffer[0x1]);
        buffer[0x1] = s10(0x1, tmp);

        let tmp = buffer[0x02];
        buffer[0x2] = s10(0x2, buffer[0xa]);
        buffer[0xa] = s10(0xa, tmp);
        let tmp = buffer[0x06];
        buffer[0x6] = s10(0x6, buffer[0xe]);
        buffer[0xe] = s10(0xe, tmp);

        let tmp = buffer[0x3];
        buffer[0x3] = s10(0x3, buffer[0x7]);
        buffer[0x7] = s10(0x7, buffer[0xb]);
        buffer[0xb] = s10(0xb, buffer[0xf]);
        buffer[0xf] = s10(0xf, tmp);

        let (dst, prev): (usize, &[u8]) = if mode == 3 {
            if i < 7 {
                (
                    0x70 - 0x10 * i,
                    &message_in[0x70 - 0x10 * i..0x80 - 0x10 * i],
                )
            } else {
                (0x70 - 0x10 * i, message_iv(mode))
            }
        } else if i > 0 {
            (0x10 * i, &message_in[0x10 * i..0x10 * i + 16])
        } else {
            (0, message_iv(mode))
        };
        for k in 0..16 {
            out[dst + k] = buffer[k] ^ prev[k];
        }
    }
}

fn generate_session_key(old_sap: &[u8], message_in: &[u8; 164]) -> [u8; 16] {
    let mut decrypted = [0u8; 128];
    decrypt_message(message_in, &mut decrypted);
    let mut new_sap = [0u8; 320];
    new_sap[0x000..0x011].copy_from_slice(&STATIC_SOURCE_1);
    new_sap[0x011..0x091].copy_from_slice(&decrypted);
    new_sap[0x091..0x111].copy_from_slice(&old_sap[0x80..0x100]);
    new_sap[0x111..0x140].copy_from_slice(&STATIC_SOURCE_2[..0x2f]);
    let mut session_key = INITIAL_SESSION_KEY;

    for round in 0..5usize {
        let mut base = [0u8; 64];
        base.copy_from_slice(&new_sap[round * 64..round * 64 + 64]);
        let md5 = modified_md5(&base, &session_key);
        sap_hash(&base, &mut session_key);
        for i in 0..4 {
            let sum = le_word(&session_key[4 * i..]).wrapping_add(le_word(&md5[4 * i..]));
            session_key[4 * i..4 * i + 4].copy_from_slice(&sum.to_le_bytes());
        }
    }
    for chunk in session_key.chunks_exact_mut(4) {
        chunk.swap(0, 3);
        chunk.swap(1, 2);
    }
    for b in &mut session_key {
        *b ^= 121;
    }
    session_key
}

pub(super) fn playfair_decrypt(message3: &[u8; 164], cipher_text: &[u8; 72]) -> [u8; 16] {
    let session_key = generate_session_key(&DEFAULT_SAP, message3);
    let schedule = generate_key_schedule(&session_key);
    let mut block_in = [0u8; 16];
    block_in.copy_from_slice(&cipher_text[56..72]);
    xor16(&mut block_in, &Z_KEY);
    cycle(&mut block_in, &schedule);
    let mut key_out = [0u8; 16];
    for i in 0..16 {
        key_out[i] = block_in[i] ^ cipher_text[16 + i];
    }
    xor16(&mut key_out, &X_KEY);
    xor16(&mut key_out, &Z_KEY);
    key_out
}
