use crate::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub const LEN: usize = 12;

    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::LEN {
            return Err(Error::protocol("paquete RTP demasiado corto"));
        }
        if buf[0] >> 6 != 2 {
            return Err(Error::protocol("versión RTP no válida"));
        }
        Ok(Self {
            marker: buf[1] & 0x80 != 0,
            payload_type: buf[1] & 0x7f,
            sequence: u16::from_be_bytes([buf[2], buf[3]]),
            timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ssrc: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        })
    }

    pub fn write(&self, buf: &mut [u8; Self::LEN]) {
        buf[0] = 0x80;
        buf[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7f);
        buf[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }
}

pub const RESEND_REQUEST_LEN: usize = 8;

pub fn encode_resend_request(
    control_sequence: u16,
    first: u16,
    count: u16,
    out: &mut [u8; RESEND_REQUEST_LEN],
) {
    out[0] = 0x80;
    out[1] = 0x55 | 0x80;
    out[2..4].copy_from_slice(&control_sequence.to_be_bytes());
    out[4..6].copy_from_slice(&first.to_be_bytes());
    out[6..8].copy_from_slice(&count.to_be_bytes());
}

fn distance(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

pub struct JitterBuffer {
    slot_size: usize,
    capacity: usize,
    mask: usize,
    data: Vec<u8>,
    lens: Vec<usize>,
    stamps: Vec<u32>,
    seqs: Vec<u16>,
    filled: Vec<bool>,
    started: bool,
    next: u16,
    end: u16,
    skip_after: u16,
    skipped: u64,
}

impl JitterBuffer {
    pub fn new(capacity: usize, slot_size: usize) -> Self {
        let capacity = capacity.clamp(2, 1 << 14).next_power_of_two();
        Self {
            slot_size,
            capacity,
            mask: capacity - 1,
            data: vec![0; capacity * slot_size],
            lens: vec![0; capacity],
            stamps: vec![0; capacity],
            seqs: vec![0; capacity],
            filled: vec![false; capacity],
            started: false,
            next: 0,
            end: 0,
            skip_after: (capacity / 2) as u16,
            skipped: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn slot_size(&self) -> usize {
        self.slot_size
    }

    pub fn set_skip_after(&mut self, packets: u16) {
        self.skip_after = packets.clamp(1, (self.capacity - 1) as u16);
    }

    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    pub fn next_sequence(&self) -> Option<u16> {
        self.started.then_some(self.next)
    }

    pub fn span(&self) -> usize {
        if self.started {
            usize::from(self.end.wrapping_sub(self.next))
        } else {
            0
        }
    }

    pub fn insert(&mut self, sequence: u16, timestamp: u32, payload: &[u8]) -> Result<()> {
        if payload.len() > self.slot_size {
            return Err(Error::protocol(
                "la carga RTP no cabe en el hueco del búfer",
            ));
        }
        if !self.started {
            self.started = true;
            self.next = sequence;
            self.end = sequence;
        }
        let ahead = distance(sequence, self.next);
        if ahead < 0 {
            return Ok(());
        }
        let ahead = ahead as usize;
        if ahead >= self.capacity {
            let new_next = sequence.wrapping_sub((self.capacity - 1) as u16);
            let dropped = usize::from(new_next.wrapping_sub(self.next)).min(self.capacity);
            for i in 0..dropped {
                let slot = usize::from(self.next.wrapping_add(i as u16)) & self.mask;
                self.filled[slot] = false;
            }
            self.skipped += dropped as u64;
            self.next = new_next;
        }
        let slot = usize::from(sequence) & self.mask;
        if self.filled[slot] && self.seqs[slot] == sequence {
            return Ok(());
        }
        let start = slot * self.slot_size;
        self.data[start..start + payload.len()].copy_from_slice(payload);
        self.lens[slot] = payload.len();
        self.stamps[slot] = timestamp;
        self.seqs[slot] = sequence;
        self.filled[slot] = true;
        if distance(sequence.wrapping_add(1), self.end) > 0 {
            self.end = sequence.wrapping_add(1);
        }
        Ok(())
    }

    fn is_filled(&self, sequence: u16) -> bool {
        let slot = usize::from(sequence) & self.mask;
        self.filled[slot] && self.seqs[slot] == sequence
    }

    pub fn pop(&mut self) -> Option<(u32, &[u8])> {
        if !self.started || self.next == self.end {
            return None;
        }
        if !self.is_filled(self.next) {
            if self.span() <= usize::from(self.skip_after) {
                return None;
            }
            while self.next != self.end && !self.is_filled(self.next) {
                self.next = self.next.wrapping_add(1);
                self.skipped += 1;
            }
            if self.next == self.end {
                return None;
            }
        }
        let slot = usize::from(self.next) & self.mask;
        self.filled[slot] = false;
        self.next = self.next.wrapping_add(1);
        let start = slot * self.slot_size;
        Some((
            self.stamps[slot],
            &self.data[start..start + self.lens[slot]],
        ))
    }

    pub fn missing(&self) -> Option<(u16, u16)> {
        if !self.started || self.next == self.end || self.is_filled(self.next) {
            return None;
        }
        let mut count = 0u16;
        let mut seq = self.next;
        while seq != self.end && !self.is_filled(seq) {
            count += 1;
            seq = seq.wrapping_add(1);
        }
        Some((self.next, count))
    }

    pub fn flush(&mut self, next_sequence: Option<u16>) {
        self.filled.fill(false);
        match next_sequence {
            Some(seq) => {
                self.started = true;
                self.next = seq;
                self.end = seq;
            }
            None => {
                self.started = false;
                self.next = 0;
                self.end = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(buffer: &mut JitterBuffer) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some((ts, payload)) = buffer.pop() {
            out.push((ts, payload.to_vec()));
        }
        out
    }

    #[test]
    fn header_roundtrip() {
        let header = RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 0xBEEF,
            timestamp: 0x0102_0304,
            ssrc: 0xAABB_CCDD,
        };
        let mut raw = [0u8; RtpHeader::LEN];
        header.write(&mut raw);
        assert_eq!(raw[0], 0x80);
        assert_eq!(raw[1], 0xE0);
        assert_eq!(RtpHeader::parse(&raw).unwrap(), header);
    }

    #[test]
    fn header_rejects_short_and_bad_version() {
        assert!(RtpHeader::parse(&[0x80; 11]).is_err());
        assert!(RtpHeader::parse(&[0x00; 12]).is_err());
    }

    #[test]
    fn resend_request_layout() {
        let mut out = [0u8; RESEND_REQUEST_LEN];
        encode_resend_request(0x0102, 0xFFFE, 3, &mut out);
        assert_eq!(out, [0x80, 0xD5, 0x01, 0x02, 0xFF, 0xFE, 0x00, 0x03]);
    }

    #[test]
    fn in_order_delivery() {
        let mut jb = JitterBuffer::new(8, 16);
        for seq in 10..14u16 {
            jb.insert(seq, u32::from(seq) * 352, &[seq as u8]).unwrap();
        }
        let got = drain(&mut jb);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], (3520, vec![10]));
        assert_eq!(got[3], (4576, vec![13]));
        assert!(jb.pop().is_none());
    }

    #[test]
    fn reorders_and_ignores_duplicates() {
        let mut jb = JitterBuffer::new(8, 16);
        jb.insert(1, 1, &[1]).unwrap();
        jb.insert(3, 3, &[3]).unwrap();
        assert_eq!(jb.pop().unwrap().1, &[1]);
        assert!(jb.pop().is_none());
        assert_eq!(jb.missing(), Some((2, 1)));
        jb.insert(2, 2, &[2]).unwrap();
        jb.insert(2, 2, &[9]).unwrap();
        jb.insert(1, 1, &[1]).unwrap();
        let got: Vec<u8> = drain(&mut jb).into_iter().map(|(_, p)| p[0]).collect();
        assert_eq!(got, vec![2, 3]);
        assert_eq!(jb.missing(), None);
    }

    #[test]
    fn wraparound() {
        let mut jb = JitterBuffer::new(8, 4);
        for seq in [0xFFFE_u16, 0xFFFF, 0, 1, 2] {
            jb.insert(seq, 0, &[seq as u8]).unwrap();
        }
        let got: Vec<u8> = drain(&mut jb).into_iter().map(|(_, p)| p[0]).collect();
        assert_eq!(got, vec![0xFE, 0xFF, 0, 1, 2]);
        assert_eq!(jb.next_sequence(), Some(3));
    }

    #[test]
    fn wraparound_with_gap() {
        let mut jb = JitterBuffer::new(8, 4);
        jb.insert(0xFFFF, 0, &[1]).unwrap();
        jb.insert(1, 0, &[3]).unwrap();
        assert_eq!(jb.pop().unwrap().1, &[1]);
        assert_eq!(jb.missing(), Some((0, 1)));
        jb.insert(0, 0, &[2]).unwrap();
        assert_eq!(drain(&mut jb).len(), 2);
    }

    #[test]
    fn gap_is_skipped_after_enough_packets() {
        let mut jb = JitterBuffer::new(16, 4);
        jb.set_skip_after(3);
        jb.insert(0, 0, &[0]).unwrap();
        assert_eq!(jb.pop().unwrap().1, &[0]);
        for seq in 2..4u16 {
            jb.insert(seq, 0, &[seq as u8]).unwrap();
        }
        assert!(jb.pop().is_none());
        assert_eq!(jb.missing(), Some((1, 1)));
        jb.insert(4, 0, &[4]).unwrap();
        let got: Vec<u8> = drain(&mut jb).into_iter().map(|(_, p)| p[0]).collect();
        assert_eq!(got, vec![2, 3, 4]);
        assert_eq!(jb.skipped(), 1);
    }

    #[test]
    fn late_packets_are_dropped() {
        let mut jb = JitterBuffer::new(8, 4);
        jb.insert(5, 0, &[5]).unwrap();
        assert!(jb.pop().is_some());
        jb.insert(4, 0, &[4]).unwrap();
        jb.insert(5, 0, &[5]).unwrap();
        assert!(jb.pop().is_none());
    }

    #[test]
    fn far_future_packet_slides_window() {
        let mut jb = JitterBuffer::new(8, 4);
        jb.insert(0, 0, &[0]).unwrap();
        jb.insert(100, 0, &[100]).unwrap();
        let got: Vec<u8> = drain(&mut jb).into_iter().map(|(_, p)| p[0]).collect();
        assert_eq!(got, vec![100]);
        assert!(jb.skipped() > 0);
    }

    #[test]
    fn flush_restarts_at_given_sequence() {
        let mut jb = JitterBuffer::new(8, 4);
        jb.insert(1, 0, &[1]).unwrap();
        jb.insert(2, 0, &[2]).unwrap();
        jb.flush(Some(50));
        assert!(jb.pop().is_none());
        jb.insert(49, 0, &[0]).unwrap();
        jb.insert(50, 0, &[50]).unwrap();
        assert_eq!(drain(&mut jb).len(), 1);
        jb.flush(None);
        jb.insert(7, 0, &[7]).unwrap();
        assert_eq!(jb.next_sequence(), Some(7));
        assert_eq!(drain(&mut jb).len(), 1);
    }

    #[test]
    fn oversize_payload_is_rejected() {
        let mut jb = JitterBuffer::new(4, 2);
        assert!(jb.insert(0, 0, &[1, 2, 3]).is_err());
        assert_eq!(jb.capacity(), 4);
        assert_eq!(jb.slot_size(), 2);
    }
}
