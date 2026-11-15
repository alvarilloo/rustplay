// Playback speed for the audio: a pitch-preserving time stretch (WSOLA) on 44.1 kHz stereo, and
// the bookkeeping that turns "output frames played" back into media time, which drives the
// picture.

use std::collections::VecDeque;
use std::f32::consts::PI;

use super::pcm::OUT_RATE;

// Analysis window (about 23 ms), half of it is the output hop, and how far (about 8.7 ms) the
// best-matching piece is searched around the nominal position.
const WINDOW: usize = 1024;
const HOP: usize = WINDOW / 2;
const SEARCH: usize = 384;
// The similarity is computed on every 4th sample.
const DECIMATE: usize = 4;

pub(super) const MIN_SPEED: f64 = 0.25;
pub(super) const MAX_SPEED: f64 = 4.0;

type Frame = [f32; 2];

pub(super) struct Stretcher {
    speed: f64,
    window: Vec<f32>,
    input: Vec<Frame>,
    // Absolute index (in input frames since the start) of `input[0]`.
    base: usize,
    // Absolute position where the next analysis window nominally starts.
    next: f64,
    // Absolute start of the previously chosen window.
    previous: Option<usize>,
    // Windowed second half of the previous piece, still to be overlapped.
    tail: Vec<Frame>,
}

impl Stretcher {
    pub fn new(speed: f64) -> Self {
        let window = (0..WINDOW).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / WINDOW as f32).cos()).collect();
        Self { speed: speed.clamp(MIN_SPEED, MAX_SPEED), window, input: Vec::new(), base: 0, next: 0.0, previous: None, tail: vec![[0.0; 2]; HOP] }
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(MIN_SPEED, MAX_SPEED);
    }

    pub fn reset(&mut self) {
        self.input.clear();
        self.base = 0;
        self.next = 0.0;
        self.previous = None;
        self.tail.iter_mut().for_each(|f| *f = [0.0; 2]);
    }

    fn mono(&self, absolute: usize) -> f32 {
        let f = self.input[absolute - self.base];
        (f[0] + f[1]) * 0.5
    }

    // Start of the window that best continues the previous piece, around `center`.
    fn best_start(&self, center: usize, previous: usize) -> usize {
        let lowest = self.base.max(center.saturating_sub(SEARCH));
        let highest = center + SEARCH;
        // What the previous piece would naturally be followed by.
        let reference = previous + HOP;
        let mut best = (f32::MIN, center.max(lowest));
        for start in lowest..=highest {
            let (mut dot, mut energy) = (0.0f32, 1e-3f32);
            for i in (0..HOP).step_by(DECIMATE) {
                let c = self.mono(start + i);
                dot += self.mono(reference + i) * c;
                energy += c * c;
            }
            let score = dot / energy.sqrt();
            if score > best.0 {
                best = (score, start);
            }
        }
        best.1
    }

    // Stretches interleaved stereo `input` and appends the result to `out`. The output is
    // delayed by a few tens of milliseconds, which the caller does not need to know about.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<i16>) {
        self.input.extend(input.chunks_exact(2).map(|f| [f32::from(f[0]) / 32768.0, f32::from(f[1]) / 32768.0]));
        loop {
            let center = self.next.round() as usize;
            let end = self.base + self.input.len();
            if center + SEARCH + WINDOW > end || center < self.base {
                if center < self.base {
                    // Everything before the buffer is gone (a large speed-up): resume from its start.
                    self.next = self.base as f64;
                    continue;
                }
                break;
            }
            let start = match self.previous {
                Some(previous) => self.best_start(center, previous),
                None => center,
            };
            let first = self.previous.is_none();
            let piece = &self.input[start - self.base..start - self.base + WINDOW];
            for (i, p) in piece.iter().take(HOP).enumerate() {
                let frame = if first { *p } else { [self.tail[i][0] + p[0] * self.window[i], self.tail[i][1] + p[1] * self.window[i]] };
                out.push((frame[0] * 32768.0).round().clamp(-32768.0, 32767.0) as i16);
                out.push((frame[1] * 32768.0).round().clamp(-32768.0, 32767.0) as i16);
            }
            for i in 0..HOP {
                let (f, w) = (piece[HOP + i], self.window[HOP + i]);
                self.tail[i] = [f[0] * w, f[1] * w];
            }
            self.previous = Some(start);
            self.next += HOP as f64 * self.speed;
        }
        // Forget what no later window can need.
        let keep_from = self.previous.map_or(0, |p| p + HOP).min((self.next.round() as usize).saturating_sub(SEARCH));
        if keep_from > self.base {
            let drop = (keep_from - self.base).min(self.input.len());
            self.input.drain(..drop);
            self.base += drop;
        }
    }
}

// Turns the number of output frames the device has played into media time, when the speed (and
// so the media time per output frame) changed along the way.
#[derive(Default)]
pub(super) struct Ledger {
    chunks: VecDeque<(u64, f64)>,
    done_frames: u64,
    done_us: f64,
}

impl Ledger {
    // `frames` output frames were queued at `speed`.
    pub fn push(&mut self, frames: u64, speed: f64) {
        let per_frame = 1_000_000.0 * speed / f64::from(OUT_RATE);
        match self.chunks.back_mut() {
            Some(last) if last.1 == per_frame => last.0 += frames,
            _ => self.chunks.push_back((frames, per_frame)),
        }
    }

    // Media microseconds that have gone by once `played` output frames came out of the device.
    pub fn media_us(&mut self, played: u64) -> f64 {
        let mut rest = played.saturating_sub(self.done_frames);
        while let Some(&(frames, per_frame)) = self.chunks.front() {
            if rest < frames {
                break;
            }
            rest -= frames;
            self.done_frames += frames;
            self.done_us += frames as f64 * per_frame;
            self.chunks.pop_front();
        }
        self.done_us + rest as f64 * self.chunks.front().map_or(0.0, |c| c.1)
    }
}

// The speeds offered to the user, and the neighbour of a speed for the `<` and `>` shortcuts.
pub(crate) const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

pub(crate) fn step_speed(current: f64, up: bool) -> f64 {
    if up {
        SPEEDS.iter().copied().find(|&s| s > current + 1e-6).unwrap_or(SPEEDS[SPEEDS.len() - 1])
    } else {
        SPEEDS.iter().rev().copied().find(|&s| s < current - 1e-6).unwrap_or(SPEEDS[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f64, hz: f64) -> Vec<i16> {
        let frames = (seconds * f64::from(OUT_RATE)) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = ((i as f64 * hz * 2.0 * std::f64::consts::PI / f64::from(OUT_RATE)).sin() * 8000.0) as i16;
                [v, v / 2]
            })
            .collect()
    }

    // Frequency from rising zero crossings of the left channel.
    fn pitch(samples: &[i16]) -> f64 {
        let left: Vec<i16> = samples.chunks_exact(2).map(|f| f[0]).collect();
        let crossings = left.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        crossings as f64 / (left.len() as f64 / f64::from(OUT_RATE))
    }

    fn run(speed: f64, chunk: usize) -> Vec<i16> {
        let input = tone(3.0, 440.0);
        let mut s = Stretcher::new(speed);
        let mut out = Vec::new();
        for part in input.chunks(chunk * 2) {
            s.process(part, &mut out);
        }
        out
    }

    #[test]
    fn faster_playback_is_shorter_and_keeps_the_pitch() {
        for speed in [1.25, 1.5, 2.0] {
            let out = run(speed, 1024);
            let seconds = out.len() as f64 / 2.0 / f64::from(OUT_RATE);
            assert!((seconds - 3.0 / speed).abs() < 0.12, "{speed}x: {seconds:.3} s");
            let hz = pitch(&out);
            assert!((hz - 440.0).abs() < 22.0, "{speed}x: pitch {hz:.1} Hz");
        }
    }

    #[test]
    fn slower_playback_is_longer_and_keeps_the_pitch() {
        for speed in [0.5, 0.75] {
            let out = run(speed, 777);
            let seconds = out.len() as f64 / 2.0 / f64::from(OUT_RATE);
            assert!((seconds - 3.0 / speed).abs() < 0.15, "{speed}x: {seconds:.3} s");
            let hz = pitch(&out);
            assert!((hz - 440.0).abs() < 22.0, "{speed}x: pitch {hz:.1} Hz");
        }
    }

    #[test]
    fn the_stretched_tone_has_no_clicks() {
        let out = run(1.5, 1024);
        // A 440 Hz sine of amplitude 8000 never moves more than about 500 per sample.
        let worst = out.chunks_exact(2).map(|f| f[0]).collect::<Vec<_>>().windows(2).map(|w| (i32::from(w[1]) - i32::from(w[0])).abs()).max().unwrap();
        assert!(worst < 900, "largest step {worst}");
        let peak = out.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((7000..=9000).contains(&peak), "overlap-add changed the level: {peak}");
    }

    #[test]
    fn the_speed_can_change_while_playing() {
        let input = tone(4.0, 440.0);
        let mut s = Stretcher::new(1.0);
        let mut out = Vec::new();
        for (i, part) in input.chunks(2048).enumerate() {
            if i == 20 {
                s.set_speed(2.0);
            }
            s.process(part, &mut out);
        }
        // About 0.93 s at 1x (20 chunks) then the rest at 2x.
        let seconds = out.len() as f64 / 2.0 / f64::from(OUT_RATE);
        let expected = 20.0 * 1024.0 / 44_100.0 + (4.0 - 20.0 * 1024.0 / 44_100.0) / 2.0;
        assert!((seconds - expected).abs() < 0.12, "{seconds:.3} vs {expected:.3}");
        assert!((pitch(&out) - 440.0).abs() < 22.0);
    }

    #[test]
    fn the_ledger_converts_played_frames_to_media_time() {
        let mut l = Ledger::default();
        // One second of output at 1x, then one second at 2x.
        l.push(u64::from(OUT_RATE), 1.0);
        l.push(u64::from(OUT_RATE), 2.0);
        assert_eq!(l.media_us(0), 0.0);
        assert!((l.media_us(u64::from(OUT_RATE) / 2) - 500_000.0).abs() < 1.0);
        assert!((l.media_us(u64::from(OUT_RATE)) - 1_000_000.0).abs() < 1.0);
        // Half a second into the 2x part is one media second further.
        assert!((l.media_us(u64::from(OUT_RATE) * 3 / 2) - 2_000_000.0).abs() < 1.0);
        assert!((l.media_us(u64::from(OUT_RATE) * 2) - 3_000_000.0).abs() < 1.0);
        // Same speed in a row merges into one chunk.
        l.push(10, 2.0);
        l.push(10, 2.0);
        assert_eq!(l.chunks.len(), 1);
    }

    #[test]
    fn speed_steps_walk_the_menu_list() {
        assert_eq!(step_speed(1.0, true), 1.25);
        assert_eq!(step_speed(1.0, false), 0.75);
        assert_eq!(step_speed(2.0, true), 2.0);
        assert_eq!(step_speed(0.5, false), 0.5);
        assert_eq!(step_speed(1.1, true), 1.25, "an unlisted speed snaps to its upper neighbour first");
    }
}
