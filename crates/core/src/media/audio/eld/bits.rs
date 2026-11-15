// Ported from FFmpeg libavcodec/aac (LGPL-2.1-or-later)

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[cfg(test)]
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    pub fn overrun(&self) -> bool {
        self.pos > self.total_bits()
    }

    pub fn bits_left(&self) -> usize {
        self.total_bits().saturating_sub(self.pos)
    }

    // Reads past the end yield zero bits and are flagged by `overrun`.
    pub fn read(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        debug_assert!(n <= 32);
        let byte = self.pos >> 3;
        let mut v: u64 = 0;
        for i in 0..5 {
            v = (v << 8) | u64::from(self.data.get(byte + i).copied().unwrap_or(0));
        }
        let shift = 40 - (self.pos & 7) as u32 - n;
        self.pos += n as usize;
        ((v >> shift) & ((1u64 << n) - 1)) as u32
    }

    pub fn read_bit(&mut self) -> u32 {
        self.read(1)
    }

    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }
}
