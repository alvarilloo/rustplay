use aes::Aes128;
use aes::cipher::{Block, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use cbc::{Decryptor, Encryptor};
use chacha20::ChaCha20;
use chacha20::cipher::{StreamCipher, StreamCipherSeek};
use hkdf::Hkdf;
use poly1305::Poly1305;
use poly1305::universal_hash::KeyInit;
use sha2::{Digest, Sha512};

use crate::{Error, Result};

#[derive(Clone)]
pub struct AesCtrKey {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

fn sha512_16(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

// SHA-512(aes_key || ecdh_secret)[..16]. Skip this step (use the raw FairPlay key) when
// pairing did not produce an ECDH secret or the client uses the old unhashed protocol.
pub fn audio_key(aes_key: &[u8; 16], ecdh_secret: &[u8; 32]) -> [u8; 16] {
    sha512_16(&[aes_key, ecdh_secret])
}

// Key the receiver decrypts RTP audio (AES) and mirror streams with, given the raw key wrapped in
// the FairPlay `ekey`. With HAP pairing (or when the receiver was told `X-Apple-PD: 1`) the
// pair-verify secret is mixed in. Without a secret the raw key is used.
pub fn stream_master_key(
    raw_key: &[u8; 16],
    shared_secret: &[u8],
    mix_pair_verify_secret: bool,
) -> [u8; 16] {
    if !mix_pair_verify_secret || shared_secret.is_empty() {
        return *raw_key;
    }
    sha512_16(&[raw_key, shared_secret])
}

// `audio_key` is the AES key already hashed with the ECDH secret (the one used for RTP audio).
pub fn mirror_key_from_audio_key(audio_key: &[u8; 16], stream_connection_id: u64) -> AesCtrKey {
    let key_label = format!("AirPlayStreamKey{stream_connection_id}");
    let iv_label = format!("AirPlayStreamIV{stream_connection_id}");
    AesCtrKey {
        key: sha512_16(&[key_label.as_bytes(), audio_key]),
        iv: sha512_16(&[iv_label.as_bytes(), audio_key]),
    }
}

pub fn mirror_key_legacy(
    aes_key: &[u8; 16],
    ecdh_secret: &[u8; 32],
    stream_connection_id: u64,
) -> Result<AesCtrKey> {
    Ok(mirror_key_from_audio_key(
        &audio_key(aes_key, ecdh_secret),
        stream_connection_id,
    ))
}

pub struct AudioCipher {
    pub key: [u8; 16],
    pub iv: [u8; 16],
}

impl AudioCipher {
    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self { key, iv }
    }

    // Sender direction: same IV for every packet, whole blocks only, tail left in the clear.
    pub fn encrypt_in_place(&self, payload: &mut [u8]) {
        let encrypted = payload.len() & !15;
        let (blocks, _) = Block::<Aes128>::slice_as_chunks_mut(&mut payload[..encrypted]);
        Encryptor::<Aes128>::new(&self.key.into(), &self.iv.into()).encrypt_blocks(blocks);
    }

    // The IV is reused for every packet; only whole 16-byte blocks are decrypted and the
    // trailing bytes are left untouched.
    pub fn decrypt_in_place(&self, payload: &mut [u8]) {
        let encrypted = payload.len() & !15;
        let (blocks, _) = Block::<Aes128>::slice_as_chunks_mut(&mut payload[..encrypted]);
        Decryptor::<Aes128>::new(&self.key.into(), &self.iv.into()).decrypt_blocks(blocks);
    }
}

pub(crate) fn hkdf_sha512(secret: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    Hkdf::<Sha512>::new(Some(salt), secret)
        .expand(info, &mut out)
        .map_err(|_| Error::Crypto("HKDF: longitud de salida no válida".into()))?;
    Ok(out)
}

// `output` selects the sender-to-receiver direction ("Output" from the sender side).
pub fn data_stream_key(
    shared_secret: &[u8],
    stream_connection_id: u64,
    output: bool,
) -> Result<[u8; 32]> {
    let salt = format!("DataStream-Salt{stream_connection_id}");
    let info: &[u8] = if output {
        b"DataStream-Output-Encryption-Key"
    } else {
        b"DataStream-Input-Encryption-Key"
    };
    hkdf_sha512(shared_secret, salt.as_bytes(), info)
}

// Returns (write, read) as named by the controller (sender). A receiver reads with the
// first key and writes with the second one; the sender writes with the first and reads with the
// second (`HapCodec::enable(read, write)` takes them in that order).
pub fn control_keys(shared_secret: &[u8; 32]) -> Result<([u8; 32], [u8; 32])> {
    control_keys_from(shared_secret)
}

// Same derivation for key material of any length (the 64-byte SRP key after transient pairing).
pub fn control_keys_from(shared_secret: &[u8]) -> Result<([u8; 32], [u8; 32])> {
    Ok((
        hkdf_sha512(
            shared_secret,
            b"Control-Salt",
            b"Control-Write-Encryption-Key",
        )?,
        hkdf_sha512(
            shared_secret,
            b"Control-Salt",
            b"Control-Read-Encryption-Key",
        )?,
    ))
}

// Event channel keys for the sender as (read, write). The direction is reversed with respect to
// the control channel: the receiver's requests use the "Write" key and our replies the "Read" one.
pub fn event_keys(shared_secret: &[u8]) -> Result<([u8; 32], [u8; 32])> {
    Ok((
        hkdf_sha512(
            shared_secret,
            b"Events-Salt",
            b"Events-Write-Encryption-Key",
        )?,
        hkdf_sha512(shared_secret, b"Events-Salt", b"Events-Read-Encryption-Key")?,
    ))
}

const AUDIO_NONCE_LEN: usize = 8;
const AUDIO_TAG_LEN: usize = 16;

// Equal to RFC 8439 ChaCha20-Poly1305 with nonce = 4 zero bytes | le64(counter), as doubletake's
// TestAudioChaCha64AEADEquivalentToZeroPrefixedIETF asserts; `shk` is chosen by the sender.
pub struct AudioSealer {
    key: [u8; 32],
    scratch: Vec<u8>,
}

impl AudioSealer {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            scratch: Vec::with_capacity(2048),
        }
    }

    fn cipher(&self, counter: u64) -> ChaCha20 {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&counter.to_le_bytes());
        ChaCha20::new(&self.key.into(), &nonce.into())
    }

    fn tag(&mut self, poly_key: &[u8; 32], aad: &[u8], ciphertext: &[u8]) -> [u8; AUDIO_TAG_LEN] {
        let pad = |len: usize| (16 - len % 16) % 16;
        self.scratch.clear();
        self.scratch.extend_from_slice(aad);
        self.scratch.resize(self.scratch.len() + pad(aad.len()), 0);
        self.scratch.extend_from_slice(ciphertext);
        self.scratch
            .resize(self.scratch.len() + pad(ciphertext.len()), 0);
        self.scratch
            .extend_from_slice(&(aad.len() as u64).to_le_bytes());
        self.scratch
            .extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
        Poly1305::new(poly_key.into())
            .compute_unpadded(&self.scratch)
            .into()
    }

    // On entry `payload` is the plaintext; on exit `ciphertext | tag(16) | counter(8, LE)`, which
    // goes right after the 12-byte RTP `header`. The AAD is header[4..12] (timestamp and SSRC).
    pub fn seal_in_place(
        &mut self,
        header: &[u8; 12],
        payload: &mut Vec<u8>,
        counter: u64,
    ) -> Result<()> {
        self.seal_with_clear_prefix(header, payload, 0, counter)
    }

    // Like `seal_in_place`, leaving the first `clear_prefix` payload bytes (an RFC 2198 header)
    // unencrypted and outside the MAC.
    pub fn seal_with_clear_prefix(
        &mut self,
        header: &[u8; 12],
        payload: &mut Vec<u8>,
        clear_prefix: usize,
        counter: u64,
    ) -> Result<()> {
        if clear_prefix > payload.len() {
            return Err(Error::Crypto("prefijo en claro mayor que la carga".into()));
        }
        let mut cipher = self.cipher(counter);
        let mut poly_key = [0u8; 32];
        cipher.apply_keystream(&mut poly_key);
        cipher.seek(64u32);
        cipher.apply_keystream(&mut payload[clear_prefix..]);
        let tag = self.tag(&poly_key, &header[4..12], &payload[clear_prefix..]);
        payload.extend_from_slice(&tag);
        payload.extend_from_slice(&counter.to_le_bytes());
        Ok(())
    }

    // Inverse of `seal_with_clear_prefix` (`payload` is `prefix | ciphertext | tag | counter`);
    // returns the counter.
    pub fn open_in_place(
        &mut self,
        header: &[u8; 12],
        payload: &mut Vec<u8>,
        clear_prefix: usize,
    ) -> Result<u64> {
        let trailer = AUDIO_TAG_LEN + AUDIO_NONCE_LEN;
        if payload.len() < clear_prefix + trailer {
            return Err(Error::Crypto("paquete de audio demasiado corto".into()));
        }
        let end = payload.len() - trailer;
        let mut tag = [0u8; AUDIO_TAG_LEN];
        tag.copy_from_slice(&payload[end..end + AUDIO_TAG_LEN]);
        let mut counter = [0u8; AUDIO_NONCE_LEN];
        counter.copy_from_slice(&payload[end + AUDIO_TAG_LEN..]);
        let counter = u64::from_le_bytes(counter);
        payload.truncate(end);

        let mut cipher = self.cipher(counter);
        let mut poly_key = [0u8; 32];
        cipher.apply_keystream(&mut poly_key);
        let expected = self.tag(&poly_key, &header[4..12], &payload[clear_prefix..]);
        if expected != tag {
            return Err(Error::Crypto("etiqueta de audio no válida".into()));
        }
        cipher.seek(64u32);
        cipher.apply_keystream(&mut payload[clear_prefix..]);
        Ok(counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn seq<const N: usize>(start: u8) -> [u8; N] {
        std::array::from_fn(|i| start.wrapping_add(i as u8))
    }

    // Expected values below were computed independently with Python hashlib/hmac/cryptography.
    #[test]
    fn audio_key_is_truncated_sha512() {
        let key = audio_key(&seq(0), &seq(100));
        assert_eq!(key.to_vec(), unhex("8bb46dd24d1d37bfbad9ccd07edb7828"));
    }

    #[test]
    fn mirror_key_matches_known_answer() {
        let derived = mirror_key_legacy(&seq(0), &seq(100), 1_234_567_890_123).unwrap();
        assert_eq!(
            derived.key.to_vec(),
            unhex("ae2f74753421f7611beb43809ba69539")
        );
        assert_eq!(
            derived.iv.to_vec(),
            unhex("5e819bcf1e745006d782e6ef9677c869")
        );
        let from_audio =
            mirror_key_from_audio_key(&audio_key(&seq(0), &seq(100)), 1_234_567_890_123);
        assert_eq!(from_audio.key, derived.key);
    }

    #[test]
    fn data_stream_keys_match_known_answers() {
        let secret: [u8; 32] = seq(0);
        let out = data_stream_key(&secret, 1_234_567_890_123, true).unwrap();
        let inp = data_stream_key(&secret, 1_234_567_890_123, false).unwrap();
        assert_eq!(
            out.to_vec(),
            unhex("9307fb9842674fe68d821434309568d6b74c1e0ee97b1019af355ae52507a2aa")
        );
        assert_eq!(
            inp.to_vec(),
            unhex("a80427e518a714483f7f8a7d9aaf302e0c8909fdd02b38f4bb3faffe06eff42d")
        );
    }

    #[test]
    fn event_keys_are_distinct_from_control_keys_and_mirror_each_other() {
        let secret = seq::<32>(0);
        let (read, write) = event_keys(&secret).unwrap();
        let (control_write, control_read) = control_keys(&secret).unwrap();
        assert_ne!(read, write);
        assert!(read != control_read && read != control_write);
        assert!(write != control_read && write != control_write);
        let (read_again, write_again) = event_keys(&secret).unwrap();
        assert_eq!((read, write), (read_again, write_again));
        let (other_read, _) = event_keys(&seq::<64>(1)).unwrap();
        assert_ne!(read, other_read);
    }

    #[test]
    fn control_keys_match_known_answers() {
        let (write, read) = control_keys(&seq(0)).unwrap();
        assert_eq!(
            write.to_vec(),
            unhex("c3ca130c7033dbe5e7ff7f91d117ead869bac476994c7a48ca170c111136ed96")
        );
        assert_eq!(
            read.to_vec(),
            unhex("c09403ef8aa6c5045cbd8cf9bf3e665b2caed623af2be0e87c8f80f519914d3d")
        );
    }

    #[test]
    fn audio_cipher_decrypts_whole_blocks_and_keeps_tail() {
        let cipher = AudioCipher::new(seq(0), seq(0xa0));
        let mut payload =
            unhex("43b9c056cc9c709a426c2b4267f87ffd2c7125f02eae1dcf959cf1f87dca8f3de3eaf1f8ff");
        cipher.decrypt_in_place(&mut payload);
        let expected: Vec<u8> = (0..37u32).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(payload, expected);
    }

    #[test]
    fn audio_cipher_reuses_iv_for_every_packet_and_handles_short_payloads() {
        let cipher = AudioCipher::new(seq(0), seq(0xa0));
        for _ in 0..2 {
            let mut payload = unhex("43b9c056cc9c709a426c2b4267f87ffd");
            cipher.decrypt_in_place(&mut payload);
            assert_eq!(payload[..4], [0x03, 0x0a, 0x11, 0x18]);
        }
        let mut short = [1u8, 2, 3];
        cipher.decrypt_in_place(&mut short);
        assert_eq!(short, [1, 2, 3]);
        cipher.decrypt_in_place(&mut []);
    }

    #[test]
    fn stream_master_key_mixes_only_when_requested() {
        let raw: [u8; 16] = seq(0);
        let secret: [u8; 32] = seq(100);
        assert_eq!(
            stream_master_key(&raw, &secret, true),
            audio_key(&raw, &secret)
        );
        assert_eq!(stream_master_key(&raw, &secret, false), raw);
        assert_eq!(stream_master_key(&raw, &[], true), raw);
    }

    #[test]
    fn control_keys_from_matches_fixed_size_version() {
        let secret: [u8; 32] = seq(0);
        assert_eq!(
            control_keys(&secret).unwrap(),
            control_keys_from(&secret).unwrap()
        );
        let long: [u8; 64] = seq(0);
        assert_ne!(
            control_keys_from(&long).unwrap().0,
            control_keys(&secret).unwrap().0
        );
    }

    #[test]
    fn audio_cipher_encrypt_matches_reference_and_keeps_tail() {
        let cipher = AudioCipher::new(seq(0), seq(0xa0));
        let plain: Vec<u8> = (0..37u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut data = plain.clone();
        cipher.encrypt_in_place(&mut data);
        assert_eq!(
            data,
            unhex("43b9c056cc9c709a426c2b4267f87ffd2c7125f02eae1dcf959cf1f87dca8f3de3eaf1f8ff")
        );
        cipher.decrypt_in_place(&mut data);
        assert_eq!(data, plain);
    }

    const SEAL_HEADER: [u8; 12] = [0x80, 0x60, 0, 1, 0, 0, 1, 0, 0x11, 0x22, 0x33, 0x44];

    #[test]
    fn audio_sealer_equals_zero_prefixed_ietf() {
        use chacha20poly1305::aead::{Aead, Payload};
        use chacha20poly1305::{ChaCha20Poly1305, KeyInit as _};

        let key: [u8; 32] = seq(0);
        let ietf = ChaCha20Poly1305::new(&key.into());
        let mut sealer = AudioSealer::new(key);
        for (len, counter, prefix) in [
            (37usize, 5u64, 0usize),
            (37, 0x0102_0304, 3),
            (16, 0, 0),
            (1500, 77, 0),
        ] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&counter.to_le_bytes());
            let sealed = ietf
                .encrypt(
                    &nonce.into(),
                    Payload {
                        msg: &plain[prefix..],
                        aad: &SEAL_HEADER[4..12],
                    },
                )
                .unwrap();
            let mut payload = plain.clone();
            sealer
                .seal_with_clear_prefix(&SEAL_HEADER, &mut payload, prefix, counter)
                .unwrap();
            assert_eq!(&payload[..prefix], &plain[..prefix]);
            assert_eq!(&payload[prefix..payload.len() - 8], &sealed[..]);
            assert_eq!(&payload[payload.len() - 8..], &counter.to_le_bytes());
        }
    }

    #[test]
    fn audio_sealer_round_trip_rejects_tampering_and_keeps_capacity() {
        let mut sealer = AudioSealer::new(seq(9));
        let mut opener = AudioSealer::new(seq(9));
        let mut payload = Vec::with_capacity(1500);
        for counter in 0..50u64 {
            payload.clear();
            payload.extend((0..300u32).map(|i| (i as u8) ^ (counter as u8)));
            let plain = payload.clone();
            let capacity = payload.capacity();
            sealer
                .seal_in_place(&SEAL_HEADER, &mut payload, counter)
                .unwrap();
            assert_eq!(payload.capacity(), capacity);
            assert_eq!(payload.len(), 300 + 16 + 8);
            let mut tampered = payload.clone();
            tampered[10] ^= 1;
            assert!(
                opener
                    .open_in_place(&SEAL_HEADER, &mut tampered, 0)
                    .is_err()
            );
            let mut wrong_header = SEAL_HEADER;
            wrong_header[5] ^= 1;
            assert!(
                opener
                    .open_in_place(&wrong_header, &mut payload.clone(), 0)
                    .is_err()
            );
            assert_eq!(
                opener.open_in_place(&SEAL_HEADER, &mut payload, 0).unwrap(),
                counter
            );
            assert_eq!(payload, plain);
        }
        assert!(
            sealer
                .seal_with_clear_prefix(&SEAL_HEADER, &mut vec![0; 2], 3, 0)
                .is_err()
        );
    }
}
