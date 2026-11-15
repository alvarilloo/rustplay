use bytes::{Buf, BufMut, BytesMut};
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use tokio_util::codec::{Decoder, Encoder};

use crate::{Error, Result};

const MAX_FRAME: usize = 1024;
const TAG_LEN: usize = 16;

struct Channel {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Channel {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(key.into()),
            counter: 0,
        }
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        nonce.into()
    }
}

pub struct HapCodec<C> {
    pub inner: C,
    pub enabled: bool,
    read: Option<Channel>,
    write: Option<Channel>,
    plain_in: BytesMut,
    plain_out: BytesMut,
}

impl<C> HapCodec<C> {
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            enabled: false,
            read: None,
            write: None,
            plain_in: BytesMut::new(),
            plain_out: BytesMut::new(),
        }
    }

    pub fn enable(&mut self, read_key: [u8; 32], write_key: [u8; 32]) {
        self.read = Some(Channel::new(&read_key));
        self.write = Some(Channel::new(&write_key));
        self.enabled = true;
    }

    fn read_frame(&mut self, src: &mut BytesMut) -> Result<bool> {
        if src.len() < 2 {
            return Ok(false);
        }
        let len = usize::from(u16::from_le_bytes([src[0], src[1]]));
        if len > MAX_FRAME {
            return Err(Error::Crypto("trama HAP demasiado grande".into()));
        }
        let total = 2 + len + TAG_LEN;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(false);
        }
        let channel = self
            .read
            .as_mut()
            .ok_or_else(|| Error::Crypto("canal HAP sin clave".into()))?;
        let nonce = channel.next_nonce();
        let aad = [src[0], src[1]];
        let mut tag = Tag::default();
        tag.copy_from_slice(&src[2 + len..total]);
        channel
            .cipher
            .decrypt_inout_detached(&nonce, &aad, (&mut src[2..2 + len]).into(), &tag)
            .map_err(|_| Error::Crypto("fallo de autenticación HAP".into()))?;
        self.plain_in.extend_from_slice(&src[2..2 + len]);
        src.advance(total);
        Ok(true)
    }

    fn seal(&mut self, plain: &mut BytesMut, dst: &mut BytesMut) -> Result<()> {
        let channel = self
            .write
            .as_mut()
            .ok_or_else(|| Error::Crypto("canal HAP sin clave".into()))?;
        while !plain.is_empty() {
            let n = plain.len().min(MAX_FRAME);
            let header = (n as u16).to_le_bytes();
            dst.reserve(2 + n + TAG_LEN);
            dst.put_slice(&header);
            let start = dst.len();
            dst.put_slice(&plain[..n]);
            plain.advance(n);
            let nonce = channel.next_nonce();
            let tag = channel
                .cipher
                .encrypt_inout_detached(&nonce, &header, (&mut dst[start..]).into())
                .map_err(|_| Error::Crypto("fallo de cifrado HAP".into()))?;
            dst.put_slice(&tag);
        }
        Ok(())
    }
}

impl<C: Decoder<Error = Error>> Decoder for HapCodec<C> {
    type Item = C::Item;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<C::Item>> {
        if !self.enabled {
            return self.inner.decode(src);
        }
        loop {
            if !self.plain_in.is_empty()
                && let Some(item) = self.inner.decode(&mut self.plain_in)?
            {
                return Ok(Some(item));
            }
            if !self.read_frame(src)? {
                return Ok(None);
            }
        }
    }
}

impl<I, C: Encoder<I, Error = Error>> Encoder<I> for HapCodec<C> {
    type Error = Error;

    fn encode(&mut self, item: I, dst: &mut BytesMut) -> Result<()> {
        if !self.enabled {
            return self.inner.encode(item, dst);
        }
        let mut plain = std::mem::take(&mut self.plain_out);
        plain.clear();
        let result = self
            .inner
            .encode(item, &mut plain)
            .and_then(|()| self.seal(&mut plain, dst));
        self.plain_out = plain;
        result
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    struct Raw;

    impl Decoder for Raw {
        type Item = Bytes;
        type Error = Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>> {
            Ok((!src.is_empty()).then(|| src.split().freeze()))
        }
    }

    impl Encoder<&[u8]> for Raw {
        type Error = Error;

        fn encode(&mut self, item: &[u8], dst: &mut BytesMut) -> Result<()> {
            dst.extend_from_slice(item);
            Ok(())
        }
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    // Frames computed independently with Python `cryptography` (ChaCha20Poly1305).
    const FRAME0: &str = "0b0070dd2e5dc2c6d1be610d38f51af319df980feeacfa78fb4d1fbfa6";
    const FRAME1: &str = "0c00ec329030db4e8f7e5bd7c339f3945ee1643f7a4a7e3328af1d10972c";

    #[test]
    fn disabled_codec_is_transparent() {
        let mut codec = HapCodec::new(Raw);
        let mut buf = BytesMut::new();
        codec.encode(b"abc".as_slice(), &mut buf).unwrap();
        assert_eq!(&buf[..], b"abc");
        assert_eq!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Bytes::from_static(b"abc")
        );
    }

    #[test]
    fn encode_matches_known_frames_with_incrementing_nonce() {
        let mut codec = HapCodec::new(Raw);
        codec.enable([0; 32], key());
        let mut buf = BytesMut::new();
        codec.encode(b"hello world".as_slice(), &mut buf).unwrap();
        assert_eq!(&buf[..], &unhex(FRAME0)[..]);
        buf.clear();
        codec.encode(b"second frame".as_slice(), &mut buf).unwrap();
        assert_eq!(&buf[..], &unhex(FRAME1)[..]);
    }

    #[test]
    fn decode_handles_split_and_coalesced_input() {
        let mut codec = HapCodec::new(Raw);
        codec.enable(key(), [0; 32]);
        let wire: Vec<u8> = [unhex(FRAME0), unhex(FRAME1)].concat();
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        for byte in &wire {
            buf.put_u8(*byte);
            while let Some(item) = codec.decode(&mut buf).unwrap() {
                out.push(item);
            }
        }
        assert_eq!(
            out,
            [
                Bytes::from_static(b"hello world"),
                Bytes::from_static(b"second frame")
            ]
        );

        let mut codec = HapCodec::new(Raw);
        codec.enable(key(), [0; 32]);
        let mut buf = BytesMut::from(&wire[..]);
        assert_eq!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Bytes::from_static(b"hello world")
        );
        assert_eq!(
            codec.decode(&mut buf).unwrap().unwrap(),
            Bytes::from_static(b"second frame")
        );
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn large_payload_is_split_in_1024_byte_frames_and_roundtrips() {
        let mut sender = HapCodec::new(Raw);
        sender.enable([1; 32], [2; 32]);
        let mut receiver = HapCodec::new(Raw);
        receiver.enable([2; 32], [1; 32]);
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
        let mut wire = BytesMut::new();
        sender.encode(payload.as_slice(), &mut wire).unwrap();
        assert_eq!(wire.len(), 3000 + 3 * (2 + TAG_LEN));
        assert_eq!(u16::from_le_bytes([wire[0], wire[1]]), 1024);
        let mut received = Vec::new();
        while let Some(item) = receiver.decode(&mut wire).unwrap() {
            received.extend_from_slice(&item);
        }
        assert_eq!(received, payload);
        assert!(wire.is_empty());
    }

    #[test]
    fn sender_and_receiver_pair_up_with_control_keys() {
        let secret: [u8; 32] = std::array::from_fn(|i| (i * 3) as u8);
        let (write, read) = crate::crypto::stream::control_keys(&secret).unwrap();
        let mut sender = HapCodec::new(Raw);
        sender.enable(read, write);
        let mut receiver = HapCodec::new(Raw);
        receiver.enable(write, read);
        let mut wire = BytesMut::new();
        sender.encode(b"SETUP".as_slice(), &mut wire).unwrap();
        assert_eq!(
            receiver.decode(&mut wire).unwrap().unwrap(),
            Bytes::from_static(b"SETUP")
        );
        receiver.encode(b"200 OK".as_slice(), &mut wire).unwrap();
        assert_eq!(
            sender.decode(&mut wire).unwrap().unwrap(),
            Bytes::from_static(b"200 OK")
        );
    }

    #[test]
    fn tampered_or_oversized_frames_are_rejected() {
        let mut codec = HapCodec::new(Raw);
        codec.enable(key(), [0; 32]);
        let mut wire = unhex(FRAME0);
        wire[5] ^= 1;
        assert!(codec.decode(&mut BytesMut::from(&wire[..])).is_err());

        let mut codec = HapCodec::new(Raw);
        codec.enable(key(), [0; 32]);
        assert!(
            codec
                .decode(&mut BytesMut::from(&[0x01u8, 0x04][..]))
                .is_err()
        );
    }
}
