use std::time::Duration;

use crate::rtp::{SAMPLE_RATE, SPF};

pub const PRIME_FRAMES: usize = 3;
pub const MAX_QUEUED_FRAMES: usize = 12;

pub fn frames_to_duration(frames: u64) -> Duration {
    let nanos = u128::from(frames) * 1_000_000_000 / u128::from(SAMPLE_RATE);
    Duration::from_nanos(nanos as u64)
}

pub fn duration_to_frames(elapsed: Duration) -> u64 {
    (elapsed.as_nanos() * u128::from(SAMPLE_RATE) / 1_000_000_000) as u64
}

// Offset of packet `index` from the start of the stream; computed from the index, not accumulated,
// so rounding never drifts.
pub fn packet_offset(index: u64) -> Duration {
    frames_to_duration(index * u64::from(SPF))
}

pub fn latency_samples(latency_ms: u32) -> u32 {
    (u64::from(latency_ms) * u64::from(SAMPLE_RATE) / 1000) as u32
}

// RTP time that corresponds to `elapsed` after the stream started.
pub fn rtp_at(first_timestamp: u32, elapsed: Duration) -> u32 {
    first_timestamp.wrapping_add(duration_to_frames(elapsed) as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Send,
    Silence,
    // Queue grew beyond the limit (capture clock faster than ours): drop one and send the next.
    Skip,
}

// Keeps a small cushion of captured frames before consuming them, so that capture jitter does not
// turn into inserted silence; after the queue runs dry the cushion is rebuilt.
#[derive(Debug, Default)]
pub struct Pacer {
    primed: bool,
}

impl Pacer {
    pub fn decide(&mut self, queued: usize) -> Action {
        if queued > MAX_QUEUED_FRAMES {
            return Action::Skip;
        }
        if !self.primed && queued >= PRIME_FRAMES {
            self.primed = true;
        }
        if self.primed && queued == 0 {
            self.primed = false;
        }
        if self.primed {
            Action::Send
        } else {
            Action::Silence
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_of_frames_is_exact() {
        assert_eq!(frames_to_duration(44_100), Duration::from_secs(1));
        assert_eq!(duration_to_frames(Duration::from_secs(1)), 44_100);
    }

    #[test]
    fn packet_period_is_about_8ms() {
        let period = packet_offset(1);
        assert!(period > Duration::from_micros(7_980) && period < Duration::from_micros(7_990));
    }

    #[test]
    fn offsets_do_not_drift() {
        // 44100 / 352 packets per second does not divide evenly; check a long run against frames.
        let index = 125 * 3600;
        let exact = frames_to_duration(index * 352);
        assert_eq!(packet_offset(index), exact);
        assert!(duration_to_frames(exact).abs_diff(index * 352) <= 1);
        assert_eq!(packet_offset(0), Duration::ZERO);
    }

    #[test]
    fn latency_conversion() {
        assert_eq!(latency_samples(300), 13_230);
        assert_eq!(latency_samples(250), 11_025);
        assert_eq!(latency_samples(0), 0);
    }

    #[test]
    fn rtp_time_follows_elapsed_and_wraps() {
        assert_eq!(rtp_at(1000, Duration::from_secs(2)), 1000 + 88_200);
        assert_eq!(rtp_at(u32::MAX, Duration::from_secs(1)), 44_099);
    }

    #[test]
    fn pacer_primes_before_sending_and_rebuilds_cushion() {
        let mut pacer = Pacer::default();
        assert_eq!(pacer.decide(0), Action::Silence);
        assert_eq!(pacer.decide(2), Action::Silence);
        assert_eq!(pacer.decide(3), Action::Send);
        assert_eq!(pacer.decide(1), Action::Send);
        assert_eq!(pacer.decide(0), Action::Silence);
        assert_eq!(pacer.decide(1), Action::Silence);
    }

    #[test]
    fn pacer_skips_when_queue_overflows() {
        let mut pacer = Pacer::default();
        assert_eq!(pacer.decide(MAX_QUEUED_FRAMES + 1), Action::Skip);
        assert_eq!(pacer.decide(MAX_QUEUED_FRAMES), Action::Send);
    }
}
