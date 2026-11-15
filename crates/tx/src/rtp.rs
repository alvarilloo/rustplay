use aes::Aes128;
use aes::cipher::{Block, BlockModeEncrypt, KeyIvInit};
use rustplay_core::{Error, Result};
use cbc::Encryptor;

pub const SPF: u32 = 352;
pub const SAMPLE_RATE: u32 = 44_100;
pub const HEADER_LEN: usize = 12;
pub const SYNC_LEN: usize = 20;
pub const DATA_PAYLOAD_TYPE: u8 = 0x60;
pub const SYNC_PAYLOAD_TYPE: u8 = 0x54;
pub const RESEND_REQUEST_TYPE: u8 = 0x55;
pub const RESEND_RESPONSE_TYPE: u8 = 0x56;
pub const SLOT_SIZE: usize = 2048;
pub const RING_PACKETS: usize = 512;
const EMPTY: u32 = u32::MAX;

pub fn write_header(out: &mut [u8], marker: bool, sequence: u16, timestamp: u32, ssrc: u32) {
    out[0] = 0x80;
    out[1] = (u8::from(marker) << 7) | DATA_PAYLOAD_TYPE;
    out[2..4].copy_from_slice(&sequence.to_be_bytes());
    out[4..8].copy_from_slice(&timestamp.to_be_bytes());
    out[8..12].copy_from_slice(&ssrc.to_be_bytes());
}

// `playhead` is the RTP time playing at `ntp`; `head` is the next RTP time to be produced.
pub fn write_sync(out: &mut [u8; SYNC_LEN], first: bool, playhead: u32, ntp: u64, head: u32) {
    out[0] = if first { 0x90 } else { 0x80 };
    out[1] = 0x80 | SYNC_PAYLOAD_TYPE;
    out[2..4].copy_from_slice(&7u16.to_be_bytes());
    out[4..8].copy_from_slice(&playhead.to_be_bytes());
    out[8..16].copy_from_slice(&ntp.to_be_bytes());
    out[16..20].copy_from_slice(&head.to_be_bytes());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResendRequest {
    pub request_sequence: u16,
    pub first: u16,
    pub count: u16,
}

pub fn parse_resend_request(packet: &[u8]) -> Option<ResendRequest> {
    if packet.len() != 8 || packet[0] >> 6 != 2 || packet[1] & 0x7F != RESEND_REQUEST_TYPE {
        return None;
    }
    let count = u16::from_be_bytes([packet[6], packet[7]]);
    (count > 0).then(|| ResendRequest {
        request_sequence: u16::from_be_bytes([packet[2], packet[3]]),
        first: u16::from_be_bytes([packet[4], packet[5]]),
        count,
    })
}

pub struct PacketRing {
    data: Box<[u8]>,
    lens: Box<[u16]>,
    sequences: Box<[u32]>,
}

impl PacketRing {
    pub fn new() -> Self {
        Self {
            data: vec![0; RING_PACKETS * SLOT_SIZE].into_boxed_slice(),
            lens: vec![0; RING_PACKETS].into_boxed_slice(),
            sequences: vec![EMPTY; RING_PACKETS].into_boxed_slice(),
        }
    }

    fn index(sequence: u16) -> usize {
        usize::from(sequence) % RING_PACKETS
    }

    pub fn slot_mut(&mut self, sequence: u16) -> &mut [u8] {
        let start = Self::index(sequence) * SLOT_SIZE;
        &mut self.data[start..start + SLOT_SIZE]
    }

    pub fn commit(&mut self, sequence: u16, len: usize) {
        let index = Self::index(sequence);
        self.lens[index] = len as u16;
        self.sequences[index] = u32::from(sequence);
    }

    pub fn get(&self, sequence: u16) -> Option<&[u8]> {
        let index = Self::index(sequence);
        (self.sequences[index] == u32::from(sequence))
            .then(|| &self.data[index * SLOT_SIZE..index * SLOT_SIZE + usize::from(self.lens[index])])
    }
}

impl Default for PacketRing {
    fn default() -> Self {
        Self::new()
    }
}

// Writes the 0x56 wrapper followed by the original datagram; returns the length written.
pub fn write_resend_response(out: &mut [u8], request_sequence: u16, original: &[u8]) -> usize {
    out[0] = 0x80;
    out[1] = 0x80 | RESEND_RESPONSE_TYPE;
    out[2..4].copy_from_slice(&request_sequence.to_be_bytes());
    out[4..4 + original.len()].copy_from_slice(original);
    4 + original.len()
}

// Tells the receiver that a requested packet is no longer available.
pub fn write_resend_expired(out: &mut [u8; 8], request_sequence: u16, sequence: u16) {
    out[0] = 0x80;
    out[1] = 0x80 | RESEND_RESPONSE_TYPE;
    out[2..4].copy_from_slice(&request_sequence.to_be_bytes());
    out[4..6].copy_from_slice(&sequence.to_be_bytes());
    out[6..8].fill(0);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResendOutcome {
    pub resent: u32,
    pub expired: u32,
}

// Serves a retransmit request from the ring, never answering for more than the ring can hold.
pub fn serve_resend(
    ring: &PacketRing,
    request: &ResendRequest,
    scratch: &mut [u8],
    mut send: impl FnMut(&[u8]),
) -> ResendOutcome {
    let mut outcome = ResendOutcome::default();
    let count = usize::from(request.count).min(RING_PACKETS + 1);
    for offset in 0..count {
        let sequence = request.first.wrapping_add(offset as u16);
        match ring.get(sequence) {
            Some(original) => {
                let len = write_resend_response(scratch, request.request_sequence, original);
                send(&scratch[..len]);
                outcome.resent += 1;
            }
            None => {
                let mut expired = [0u8; 8];
                write_resend_expired(&mut expired, request.request_sequence, sequence);
                send(&expired);
                outcome.expired += 1;
                break;
            }
        }
    }
    outcome
}

pub trait Protector: Send {
    // Encrypts the payload at `packet[HEADER_LEN..HEADER_LEN + payload_len]` in place and
    // returns the final length of the datagram.
    fn protect(&mut self, packet: &mut [u8], payload_len: usize) -> Result<usize>;
}

pub struct NoProtection;

impl Protector for NoProtection {
    fn protect(&mut self, _packet: &mut [u8], payload_len: usize) -> Result<usize> {
        Ok(HEADER_LEN + payload_len)
    }
}

// The IV restarts for every packet and only whole 16-byte blocks are encrypted.
pub struct AesCbcProtection {
    key: [u8; 16],
    iv: [u8; 16],
}

impl AesCbcProtection {
    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self { key, iv }
    }
}

impl Protector for AesCbcProtection {
    fn protect(&mut self, packet: &mut [u8], payload_len: usize) -> Result<usize> {
        let whole = payload_len & !15;
        let (blocks, _) = Block::<Aes128>::slice_as_chunks_mut(&mut packet[HEADER_LEN..HEADER_LEN + whole]);
        Encryptor::<Aes128>::new(&self.key.into(), &self.iv.into()).encrypt_blocks(blocks);
        Ok(HEADER_LEN + payload_len)
    }
}

pub struct PacketBuilder {
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    first: bool,
    protector: Box<dyn Protector>,
}

impl PacketBuilder {
    pub fn new(sequence: u16, timestamp: u32, ssrc: u32, protector: Box<dyn Protector>) -> Self {
        Self {
            sequence,
            timestamp,
            ssrc,
            first: true,
            protector,
        }
    }

    pub fn next_sequence(&self) -> u16 {
        self.sequence
    }

    pub fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    // Builds the next packet into `slot`; returns its sequence number and datagram length.
    pub fn build(&mut self, slot: &mut [u8], payload: &[u8]) -> Result<(u16, usize)> {
        if HEADER_LEN + payload.len() + 32 > slot.len() {
            return Err(Error::protocol("paquete de audio demasiado grande"));
        }
        let sequence = self.sequence;
        write_header(slot, false, sequence, self.timestamp, self.ssrc);
        slot[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        let len = self.protector.protect(slot, payload.len())?;
        self.first = false;
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(SPF);
        Ok((sequence, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tag;

    impl Protector for Tag {
        fn protect(&mut self, packet: &mut [u8], payload_len: usize) -> Result<usize> {
            packet[HEADER_LEN + payload_len] = 0xEE;
            Ok(HEADER_LEN + payload_len + 1)
        }
    }

    #[test]
    fn header_layout() {
        let mut out = [0u8; 12];
        write_header(&mut out, true, 0x0102, 0x0A0B_0C0D, 0x1122_3344);
        assert_eq!(out, [0x80, 0xE0, 1, 2, 0x0A, 0x0B, 0x0C, 0x0D, 0x11, 0x22, 0x33, 0x44]);
        write_header(&mut out, false, 0xFFFF, 0, 0);
        assert_eq!(out[..4], [0x80, 0x60, 0xFF, 0xFF]);
    }

    #[test]
    fn header_matches_core_parser() {
        let mut out = [0u8; 12];
        write_header(&mut out, false, 7, 352, 9);
        let header = rustplay_core::rtp::RtpHeader::parse(&out).unwrap();
        assert_eq!((header.sequence, header.timestamp, header.ssrc, header.payload_type), (7, 352, 9, 0x60));
        assert!(!header.marker);
    }

    #[test]
    fn sync_layout() {
        let mut out = [0u8; SYNC_LEN];
        write_sync(&mut out, true, 1000, 0x0102_0304_0506_0708, 14_230);
        assert_eq!(out[..4], [0x90, 0xD4, 0, 7]);
        assert_eq!(u32::from_be_bytes(out[4..8].try_into().unwrap()), 1000);
        assert_eq!(out[8..16], [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(u32::from_be_bytes(out[16..20].try_into().unwrap()), 14_230);
        write_sync(&mut out, false, 0, 0, 0);
        assert_eq!(out[0], 0x80);
    }

    #[test]
    fn resend_request_parsing() {
        let mut packet = [0u8; 8];
        rustplay_core::rtp::encode_resend_request(5, 100, 3, &mut packet);
        assert_eq!(
            parse_resend_request(&packet),
            Some(ResendRequest {
                request_sequence: 5,
                first: 100,
                count: 3
            })
        );
        packet[7] = 0;
        assert_eq!(parse_resend_request(&packet), None);
        assert_eq!(parse_resend_request(&[0x80, 0xD5, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse_resend_request(&[0x80, 0xD4, 0, 0, 0, 0, 0, 1]), None);
    }

    #[test]
    fn ring_lookup_and_overwrite() {
        let mut ring = PacketRing::new();
        assert!(ring.get(10).is_none());
        ring.slot_mut(10)[..3].copy_from_slice(&[1, 2, 3]);
        ring.commit(10, 3);
        assert_eq!(ring.get(10), Some(&[1u8, 2, 3][..]));
        let wrapped = 10 + RING_PACKETS as u16;
        ring.slot_mut(wrapped)[0] = 9;
        ring.commit(wrapped, 1);
        assert!(ring.get(10).is_none());
        assert_eq!(ring.get(wrapped), Some(&[9u8][..]));
    }

    #[test]
    fn resend_serves_available_and_reports_expired() {
        let mut ring = PacketRing::new();
        for sequence in [65_534u16, 65_535, 0] {
            ring.slot_mut(sequence)[0] = sequence as u8;
            ring.commit(sequence, 1);
        }
        let request = ResendRequest {
            request_sequence: 9,
            first: 65_534,
            count: 5,
        };
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut scratch = [0u8; 64];
        let outcome = serve_resend(&ring, &request, &mut scratch, |p| sent.push(p.to_vec()));
        assert_eq!(outcome, ResendOutcome { resent: 3, expired: 1 });
        assert_eq!(sent[0], [0x80, 0xD6, 0, 9, 0xFE]);
        assert_eq!(sent[2], [0x80, 0xD6, 0, 9, 0x00]);
        assert_eq!(sent[3], [0x80, 0xD6, 0, 9, 0, 1, 0, 0]);
        assert_eq!(sent.len(), 4);
    }

    #[test]
    fn builder_sequences_without_marker() {
        let mut builder = PacketBuilder::new(65_535, u32::MAX - 100, 7, Box::new(NoProtection));
        let mut slot = [0u8; 256];
        let (sequence, len) = builder.build(&mut slot, &[1, 2, 3]).unwrap();
        assert_eq!((sequence, len), (65_535, 15));
        assert_eq!(slot[1], 0x60);
        assert_eq!(slot[12..15], [1, 2, 3]);
        let (sequence, _) = builder.build(&mut slot, &[4]).unwrap();
        assert_eq!(sequence, 0);
        assert_eq!(slot[1], 0x60);
        assert_eq!(builder.next_timestamp(), (u32::MAX - 100).wrapping_add(2 * SPF));
        assert_eq!(builder.next_sequence(), 1);
    }

    #[test]
    fn builder_uses_protector_length() {
        let mut builder = PacketBuilder::new(0, 0, 0, Box::new(Tag));
        let mut slot = [0u8; 256];
        let (_, len) = builder.build(&mut slot, &[5, 6]).unwrap();
        assert_eq!(len, 15);
        assert_eq!(slot[14], 0xEE);
        let mut small = [0u8; 20];
        assert!(builder.build(&mut small, &[0; 16]).is_err());
    }

    #[test]
    fn aes_cbc_encrypts_whole_blocks_and_matches_receiver_cipher() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let iv: [u8; 16] = std::array::from_fn(|i| 0xA0 + i as u8);
        let plain: Vec<u8> = (0..37u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut packet = [0u8; 128];
        packet[HEADER_LEN..HEADER_LEN + 37].copy_from_slice(&plain);
        let mut protection = AesCbcProtection::new(key, iv);
        let len = protection.protect(&mut packet, 37).unwrap();
        assert_eq!(len, HEADER_LEN + 37);
        let mut payload = packet[HEADER_LEN..len].to_vec();
        assert_ne!(payload[..32], plain[..32]);
        assert_eq!(payload[32..], plain[32..]);
        rustplay_core::crypto::stream::AudioCipher::new(key, iv).decrypt_in_place(&mut payload);
        assert_eq!(payload, plain);
    }
}
