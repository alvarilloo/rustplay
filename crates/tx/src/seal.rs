use rustplay_core::Result;
use rustplay_core::crypto::stream::AudioSealer;

use crate::rtp::{HEADER_LEN, Protector};

// Datagram layout: RTP header | ciphertext | tag(16) | counter(8, LE); AAD is timestamp and SSRC.
// With `sequence_nonce` the RTP sequence number is the counter (OwnTone), so a resent packet is
// byte-identical to the original; otherwise a running counter is used.
pub struct ChaChaProtection {
    sealer: AudioSealer,
    sequence_nonce: bool,
    counter: u64,
    scratch: Vec<u8>,
}

impl ChaChaProtection {
    pub fn new(key: &[u8; 32], sequence_nonce: bool) -> Result<Self> {
        Ok(Self {
            sealer: AudioSealer::new(*key),
            sequence_nonce,
            counter: 0,
            scratch: Vec::with_capacity(crate::rtp::SLOT_SIZE),
        })
    }
}

impl Protector for ChaChaProtection {
    fn protect(&mut self, packet: &mut [u8], payload_len: usize) -> Result<usize> {
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&packet[..HEADER_LEN]);
        self.scratch.clear();
        self.scratch
            .extend_from_slice(&packet[HEADER_LEN..HEADER_LEN + payload_len]);
        let counter = if self.sequence_nonce {
            u64::from(u16::from_be_bytes([header[2], header[3]]))
        } else {
            self.counter
        };
        self.sealer.seal_in_place(&header, &mut self.scratch, counter)?;
        self.counter = self.counter.wrapping_add(1);
        let end = HEADER_LEN + self.scratch.len();
        packet[HEADER_LEN..end].copy_from_slice(&self.scratch);
        Ok(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::PacketBuilder;

    #[test]
    fn sequence_nonce_makes_identical_packets_for_the_same_sequence() {
        let key = [6u8; 32];
        let build = |builder: &mut PacketBuilder| {
            let mut slot = [0u8; 256];
            let (_, len) = builder.build(&mut slot, &[1, 2, 3]).unwrap();
            slot[..len].to_vec()
        };
        let mut a = PacketBuilder::new(300, 0, 1, Box::new(ChaChaProtection::new(&key, true).unwrap()));
        let mut b = PacketBuilder::new(300, 0, 1, Box::new(ChaChaProtection::new(&key, true).unwrap()));
        build(&mut b);
        let first = build(&mut a);
        let mut c = PacketBuilder::new(300, 0, 1, Box::new(ChaChaProtection::new(&key, true).unwrap()));
        assert_eq!(first, build(&mut c));
        let trailer = &first[first.len() - 8..];
        assert_eq!(trailer, 300u64.to_le_bytes());
    }

    #[test]
    fn packets_open_with_the_receiver_side_and_counter_increments() {
        let key = [5u8; 32];
        let mut builder = PacketBuilder::new(10, 1000, 77, Box::new(ChaChaProtection::new(&key, false).unwrap()));
        let mut opener = AudioSealer::new(key);
        for expected_counter in 0..3u64 {
            let mut slot = [0u8; 256];
            let plain: Vec<u8> = (0..50u8).collect();
            let (_, len) = builder.build(&mut slot, &plain).unwrap();
            assert_eq!(len, HEADER_LEN + 50 + 16 + 8);
            let header: [u8; 12] = slot[..12].try_into().unwrap();
            let mut payload = slot[12..len].to_vec();
            let counter = opener.open_in_place(&header, &mut payload, 0).unwrap();
            assert_eq!(counter, expected_counter);
            assert_eq!(payload, plain);
        }
    }
}
