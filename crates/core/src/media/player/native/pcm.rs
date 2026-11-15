// PCM conversion for the audio renderer, which plays 44.1 kHz stereo 16-bit.

pub(super) const OUT_RATE: u32 = 44_100;

// Interleaved samples of `channels` channels to interleaved stereo.
pub(super) fn to_stereo(input: &[i16], channels: u16, out: &mut Vec<i16>) {
    out.clear();
    match channels {
        2 => out.extend_from_slice(input),
        1 => {
            out.reserve(input.len() * 2);
            for &s in input {
                out.push(s);
                out.push(s);
            }
        }
        n if n > 2 => {
            // Keeps the front pair; good enough for the rare multichannel soundtrack.
            for frame in input.chunks_exact(usize::from(n)) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
        _ => {}
    }
}

// Linear-interpolation resampler for stereo audio, keeping its position between calls.
pub(super) struct Resampler {
    step: f64,
    pos: f64,
    prev: [i16; 2],
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        Self { step: f64::from(in_rate) / f64::from(OUT_RATE), pos: 0.0, prev: [0, 0] }
    }

    pub fn passthrough(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9
    }

    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        let n = (input.len() / 2) as isize;
        if n == 0 {
            return;
        }
        let sample = |i: isize, ch: usize, prev: &[i16; 2]| -> f64 {
            if i < 0 { f64::from(prev[ch]) } else { f64::from(input[i as usize * 2 + ch]) }
        };
        loop {
            let i = self.pos.floor() as isize;
            if i + 1 >= n {
                break;
            }
            let frac = self.pos - i as f64;
            for ch in 0..2 {
                let (a, b) = (sample(i, ch, &self.prev), sample(i + 1, ch, &self.prev));
                out.push((a + (b - a) * frac).round().clamp(-32768.0, 32767.0) as i16);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = [input[(n as usize - 1) * 2], input[(n as usize - 1) * 2 + 1]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_layouts_become_stereo() {
        let mut out = Vec::new();
        to_stereo(&[1, 2, 3], 1, &mut out);
        assert_eq!(out, [1, 1, 2, 2, 3, 3]);
        to_stereo(&[1, 2, 3, 4], 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
        to_stereo(&[1, 2, 9, 9, 3, 4, 9, 9], 4, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn resampling_keeps_the_duration_across_chunks() {
        // One second of a ramp at 48 kHz, fed in uneven chunks.
        let input: Vec<i16> = (0..48_000).flat_map(|i| [(i % 1000) as i16, -((i % 1000) as i16)]).collect();
        let mut resampler = Resampler::new(48_000);
        assert!(!resampler.passthrough());
        let mut out = Vec::new();
        let mut pos = 0;
        for size in [1000usize, 777, 4096, 31, 20000, 100_000].iter().cycle() {
            if pos >= input.len() {
                break;
            }
            let end = (pos + size * 2).min(input.len());
            resampler.process(&input[pos..end], &mut out);
            pos = end;
        }
        let frames = out.len() / 2;
        assert!((44_090..=44_100).contains(&frames), "{frames} frames");
        // Smooth input stays smooth: no jumps bigger than one slope step between neighbours.
        let jumps = out.chunks_exact(2).collect::<Vec<_>>().windows(2).filter(|w| (i32::from(w[1][0]) - i32::from(w[0][0])).abs() > 5 && w[1][0] > w[0][0]).count();
        assert_eq!(jumps, 0);
    }

    #[test]
    fn the_native_rate_is_a_passthrough() {
        assert!(Resampler::new(44_100).passthrough());
    }
}
