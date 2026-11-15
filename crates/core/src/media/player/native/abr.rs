// Adaptive quality: measures how fast segments download and decides, at segment boundaries,
// which variant of the master playlist to fetch next. The decision is a pure function so it can
// be tested without a network.

use std::collections::VecDeque;

// Step down when the measured throughput is below this multiple of the variant's BANDWIDTH.
pub(super) const DOWN_FACTOR: f64 = 1.3;
// Step up only when the next variant would still fit with this much headroom.
pub(super) const UP_FACTOR: f64 = 1.5;
// Consecutive comfortable segments needed before stepping up.
pub(super) const UP_STREAK: u32 = 3;
// Seconds of picture that must be buffered before stepping up.
pub(super) const UP_BUFFER_S: f64 = 5.0;
// Below this much buffered picture (after playback started) the stream is about to run dry.
pub(super) const DRY_BUFFER_S: f64 = 1.5;
// Download samples kept for the estimate.
const SAMPLES: usize = 3;
// Segments smaller than this say little about the link (connection set-up dominates).
const MIN_SAMPLE_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rung {
    // Advertised bits per second.
    pub bandwidth: u64,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Quality {
    #[default]
    Auto,
    // Index into the ladder (ascending quality).
    Fixed(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Decision {
    Stay,
    Switch(usize),
}

pub(super) struct Inputs<'a> {
    pub ladder: &'a [Rung],
    pub current: usize,
    // Measured download speed in bits per second.
    pub throughput_bps: Option<f64>,
    // Seconds of media downloaded but not yet shown; None until playback has started.
    pub buffer_s: Option<f64>,
    // Consecutive comfortable segments so far.
    pub streak: u32,
}

fn bandwidth(ladder: &[Rung], i: usize) -> f64 {
    ladder.get(i).map_or(1.0, |r| (r.bandwidth as f64).max(1.0))
}

// Whether the next rung up would fit comfortably (the building block of the up-streak).
pub(super) fn comfortable(i: &Inputs<'_>) -> bool {
    let Some(throughput) = i.throughput_bps else { return false };
    i.current + 1 < i.ladder.len()
        && throughput >= UP_FACTOR * bandwidth(i.ladder, i.current + 1)
        && i.buffer_s.is_none_or(|b| b >= UP_BUFFER_S)
}

pub(super) fn decide(i: &Inputs<'_>) -> Decision {
    if i.ladder.len() < 2 || i.current >= i.ladder.len() {
        return Decision::Stay;
    }
    let dry = i.buffer_s.is_some_and(|b| b < DRY_BUFFER_S);
    let slow = i.throughput_bps.is_some_and(|t| t < DOWN_FACTOR * bandwidth(i.ladder, i.current));
    if (slow || dry) && i.current > 0 {
        // Quickly: straight to the best rung that fits, at least one step down.
        let fits = |r: usize| i.throughput_bps.is_some_and(|t| t >= DOWN_FACTOR * bandwidth(i.ladder, r));
        let target = (0..i.current).rev().find(|&r| fits(r)).unwrap_or(0);
        let target = if dry && target >= i.current { i.current - 1 } else { target };
        return Decision::Switch(target.min(i.current - 1));
    }
    if comfortable(i) && i.streak >= UP_STREAK {
        return Decision::Switch(i.current + 1);
    }
    Decision::Stay
}

// Recent download speeds; the estimate is the lower of the last sample and the recent average
// (total bytes over total time), so a slowdown is noticed at once and a burst is not trusted.
#[derive(Default)]
pub(super) struct Estimator {
    samples: VecDeque<(usize, f64)>,
}

impl Estimator {
    pub fn add(&mut self, bytes: usize, seconds: f64) {
        if bytes < MIN_SAMPLE_BYTES || seconds <= 0.0 {
            return;
        }
        if self.samples.len() == SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((bytes, seconds));
    }

    pub fn bps(&self) -> Option<f64> {
        let &(bytes, seconds) = self.samples.back()?;
        let (total_bytes, total_seconds) = self.samples.iter().fold((0usize, 0.0), |(b, s), &(sb, ss)| (b + sb, s + ss));
        let last = bytes as f64 * 8.0 / seconds;
        let average = total_bytes as f64 * 8.0 / total_seconds;
        Some(last.min(average))
    }
}

// Estimator plus the up-streak, fed one finished segment at a time.
#[derive(Default)]
pub(super) struct Controller {
    estimator: Estimator,
    streak: u32,
}

impl Controller {
    pub fn record(&mut self, bytes: usize, seconds: f64) {
        self.estimator.add(bytes, seconds);
    }

    // The decision before downloading the next segment.
    pub fn next(&mut self, ladder: &[Rung], current: usize, buffer_s: Option<f64>) -> Decision {
        let mut inputs = Inputs { ladder, current, throughput_bps: self.estimator.bps(), buffer_s, streak: self.streak };
        self.streak = if comfortable(&inputs) { self.streak + 1 } else { 0 };
        inputs.streak = self.streak;
        let decision = decide(&inputs);
        if decision != Decision::Stay {
            self.streak = 0;
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> Vec<Rung> {
        vec![
            Rung { bandwidth: 400_000, height: 240 },
            Rung { bandwidth: 1_000_000, height: 360 },
            Rung { bandwidth: 2_500_000, height: 720 },
            Rung { bandwidth: 5_000_000, height: 1080 },
        ]
    }

    fn inputs(l: &[Rung], current: usize, throughput: Option<f64>, buffer: Option<f64>, streak: u32) -> Inputs<'_> {
        Inputs { ladder: l, current, throughput_bps: throughput, buffer_s: buffer, streak }
    }

    #[test]
    fn steps_down_quickly_when_the_link_is_too_slow() {
        let l = ladder();
        // 1080p needs 6.5 Mbit/s with headroom; 4 Mbit/s fits 720p (3.25) straight away.
        assert_eq!(decide(&inputs(&l, 3, Some(4_000_000.0), Some(10.0), 0)), Decision::Switch(2));
        // Very slow: all the way to the bottom.
        assert_eq!(decide(&inputs(&l, 3, Some(100_000.0), Some(10.0), 0)), Decision::Switch(0));
        // Right at the threshold stays; just under it drops.
        assert_eq!(decide(&inputs(&l, 3, Some(6_500_100.0), Some(10.0), 0)), Decision::Stay);
        assert_eq!(decide(&inputs(&l, 3, Some(6_400_000.0), Some(10.0), 0)), Decision::Switch(2));
        // Nothing below the lowest rung.
        assert_eq!(decide(&inputs(&l, 0, Some(1.0), Some(0.0), 9)), Decision::Stay);
    }

    #[test]
    fn a_dry_buffer_forces_a_step_down_even_with_good_throughput() {
        let l = ladder();
        assert_eq!(decide(&inputs(&l, 2, Some(50_000_000.0), Some(0.4), 0)), Decision::Switch(1));
        // The buffer is unknown before playback starts: no panic then.
        assert_eq!(decide(&inputs(&l, 2, Some(50_000_000.0), None, 0)), Decision::Stay);
    }

    #[test]
    fn steps_up_only_after_a_streak_with_headroom_and_buffer() {
        let l = ladder();
        let fast = Some(10_000_000.0);
        assert_eq!(decide(&inputs(&l, 1, fast, Some(12.0), UP_STREAK - 1)), Decision::Stay);
        assert_eq!(decide(&inputs(&l, 1, fast, Some(12.0), UP_STREAK)), Decision::Switch(2));
        // One step at a time, even if much faster.
        assert_eq!(decide(&inputs(&l, 0, Some(100_000_000.0), Some(12.0), 10)), Decision::Switch(1));
        // Not enough headroom for 720p (needs 3.75 Mbit/s) or not enough buffer.
        assert_eq!(decide(&inputs(&l, 1, Some(3_000_000.0), Some(12.0), 10)), Decision::Stay);
        assert_eq!(decide(&inputs(&l, 1, fast, Some(2.0), 10)), Decision::Stay);
        // Never above the top.
        assert_eq!(decide(&inputs(&l, 3, fast, Some(12.0), 10)), Decision::Stay);
        // A single rung (or none) never switches.
        assert_eq!(decide(&inputs(&l[..1], 0, fast, Some(12.0), 10)), Decision::Stay);
        assert_eq!(decide(&inputs(&[], 0, fast, Some(12.0), 10)), Decision::Stay);
    }

    #[test]
    fn the_estimate_follows_slowdowns_and_distrusts_bursts() {
        let mut e = Estimator::default();
        assert_eq!(e.bps(), None);
        e.add(1_000_000, 1.0); // 8 Mbit/s
        assert!((e.bps().unwrap() - 8e6).abs() < 1.0);
        e.add(1_000_000, 0.1); // a burst at 80 Mbit/s: the average (14.5 Mbit/s) is lower than the burst
        assert!(e.bps().unwrap() < 20e6, "{}", e.bps().unwrap());
        e.add(1_000_000, 4.0); // a slowdown to 2 Mbit/s counts at once
        assert!((e.bps().unwrap() - 2e6).abs() < 1.0);
        // Tiny downloads are ignored.
        let before = e.bps();
        e.add(100, 0.0001);
        assert_eq!(e.bps(), before);
    }

    #[test]
    fn the_controller_climbs_one_rung_per_streak_and_falls_at_once() {
        let l = ladder();
        let mut c = Controller::default();
        let mut current = 0;
        let mut switches = Vec::new();
        // 20 Mbit/s link: 2.5 MB segments of 1 s each download in 1 s, buffer healthy.
        for _ in 0..12 {
            c.record(2_500_000, 1.0);
            if let Decision::Switch(to) = c.next(&l, current, Some(10.0)) {
                switches.push(to);
                current = to;
            }
        }
        assert_eq!(switches, [1, 2, 3], "one step per {UP_STREAK} comfortable segments, never past the top");
        // The link collapses to 300 kbit/s: a single measurement is enough.
        c.record(40_000, 1.0);
        assert_eq!(c.next(&l, current, Some(10.0)), Decision::Switch(0));
    }
}
