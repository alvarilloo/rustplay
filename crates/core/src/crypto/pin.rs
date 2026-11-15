// Legacy AirPlay PIN pairing (POST /pair-pin-start + three POST /pair-setup-pin steps), as
// implemented by UxPlay's `-pin` option and Apple TV 3:
//   1. {method: "pin", user: <client device id>}  ->  {salt, pk: B}
//   2. {pk: A, proof: M1}                          ->  {proof: M2}
//   3. {epk, authTag}: the client Ed25519 key, AES-128-GCM sealed  ->  ours, sealed the same way.
// SRP-6a uses the 2048-bit RFC 5054 group, SHA-1 and Apple's 40-byte session key. The GCM key
// and the 16-byte IV come from SHA-512 over a label and the session key; the IV's last byte is
// increased once for the client message and once more for the reply.

use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
use sha2::{Digest, Sha512};

use super::srp::{Group, SrpClient, SrpServer, Variant};
use crate::{Error, Result};

pub const PIN_USER_MAX: usize = 24;
const SALT_LEN: usize = 16;
const PRIVATE_LEN: usize = 32;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;

fn strip_zeros(bytes: Vec<u8>) -> Vec<u8> {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len().saturating_sub(1));
    bytes[first..].to_vec()
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::fill(&mut out[..]);
    out
}

// A random 4-digit code, "0000".."9999".
pub fn random_pin() -> String {
    let value = u16::from_le_bytes(random::<2>()) % 10_000;
    format!("{value:04}")
}

fn gcm_keys(session_key: &[u8]) -> ([u8; 16], [u8; 16]) {
    let derive = |label: &[u8]| {
        let mut hasher = Sha512::new();
        hasher.update(label);
        hasher.update(session_key);
        let digest = hasher.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        out
    };
    let key = derive(b"Pair-Setup-AES-Key");
    let mut iv = derive(b"Pair-Setup-AES-IV");
    iv[15] = iv[15].wrapping_add(1);
    (key, iv)
}

fn next_iv(mut iv: [u8; 16]) -> [u8; 16] {
    iv[15] = iv[15].wrapping_add(1);
    iv
}

// ---------------------------------------------------------------- AES-128-GCM (any IV length)

struct Gcm {
    cipher: Aes128,
    h: u128,
}

fn gf_mul(x: u128, y: u128) -> u128 {
    const R: u128 = 0xE1 << 120;
    let mut z = 0u128;
    let mut v = y;
    for i in 0..128 {
        if (x >> (127 - i)) & 1 == 1 {
            z ^= v;
        }
        v = if v & 1 == 1 { (v >> 1) ^ R } else { v >> 1 };
    }
    z
}

impl Gcm {
    fn new(key: &[u8; 16]) -> Self {
        let cipher = Aes128::new(&Array::from(*key));
        let mut block = Array::from([0u8; 16]);
        cipher.encrypt_block(&mut block);
        let h = u128::from_be_bytes(block.into());
        Self { cipher, h }
    }

    fn ghash(&self, mut acc: u128, data: &[u8]) -> u128 {
        for chunk in data.chunks(16) {
            let mut block = [0u8; 16];
            block[..chunk.len()].copy_from_slice(chunk);
            acc = gf_mul(acc ^ u128::from_be_bytes(block), self.h);
        }
        acc
    }

    fn encrypt_block(&self, block: u128) -> u128 {
        let mut b = Array::from(block.to_be_bytes());
        self.cipher.encrypt_block(&mut b);
        u128::from_be_bytes(b.into())
    }

    fn j0(&self, iv: &[u8]) -> u128 {
        if iv.len() == 12 {
            let mut block = [0u8; 16];
            block[..12].copy_from_slice(iv);
            block[15] = 1;
            return u128::from_be_bytes(block);
        }
        let acc = self.ghash(0, iv);
        gf_mul(acc ^ (iv.len() as u128 * 8), self.h)
    }

    fn ctr(&self, j0: u128, data: &mut [u8]) {
        let mut counter = j0;
        for chunk in data.chunks_mut(16) {
            let low = (counter as u32).wrapping_add(1);
            counter = (counter & !0xFFFF_FFFF) | u128::from(low);
            let stream = self.encrypt_block(counter).to_be_bytes();
            for (b, k) in chunk.iter_mut().zip(stream) {
                *b ^= k;
            }
        }
    }

    fn tag(&self, j0: u128, ciphertext: &[u8]) -> [u8; 16] {
        let acc = self.ghash(0, ciphertext);
        let s = gf_mul(acc ^ (ciphertext.len() as u128 * 8), self.h);
        (s ^ self.encrypt_block(j0)).to_be_bytes()
    }

    fn seal(&self, iv: &[u8], data: &mut [u8]) -> [u8; 16] {
        let j0 = self.j0(iv);
        self.ctr(j0, data);
        self.tag(j0, data)
    }

    fn open(&self, iv: &[u8], data: &mut [u8], tag: &[u8]) -> bool {
        let j0 = self.j0(iv);
        let expected = self.tag(j0, data);
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(tag) {
            diff |= a ^ b;
        }
        if diff != 0 || tag.len() != TAG_LEN {
            return false;
        }
        self.ctr(j0, data);
        true
    }
}

// ---------------------------------------------------------------- receiver (accessory) side

pub struct PinConfirmed {
    // Ed25519 public key of the client, now trusted.
    pub client_key: [u8; 32],
    pub epk: [u8; 32],
    pub auth_tag: [u8; 16],
}

enum ServerState {
    Started,
    Verified { session_key: Vec<u8> },
    Done,
}

pub struct PinSetupServer {
    user: String,
    pin: String,
    salt: [u8; SALT_LEN],
    private_b: [u8; PRIVATE_LEN],
    public_b: Vec<u8>,
    state: ServerState,
}

impl PinSetupServer {
    // Step 1: `user` is the client's device id, `pin` the code shown on screen.
    pub fn start(user: &str, pin: &str) -> Result<Self> {
        if user.is_empty() || user.len() > PIN_USER_MAX {
            return Err(Error::Pairing("identificador de usuario de pair-setup-pin no válido".into()));
        }
        let mut salt = random::<SALT_LEN>();
        // The salt is hashed as a big number, so a leading zero byte would change it.
        if salt[0] == 0 {
            salt[0] = 1;
        }
        let private_b = random::<PRIVATE_LEN>();
        let group = Group::rfc5054_2048();
        let probe = SrpServer::with_variant(
            &group,
            Variant::APPLE_LEGACY,
            user.as_bytes(),
            pin.as_bytes(),
            &salt,
            &private_b,
            &[1],
        )?;
        let public_b = strip_zeros(probe.public_b(&group));
        Ok(Self {
            user: user.to_owned(),
            pin: pin.to_owned(),
            salt,
            private_b,
            public_b,
            state: ServerState::Started,
        })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public_b
    }

    // Step 2: checks the client proof M1 and returns ours (M2). Fails with a wrong PIN.
    pub fn verify(&mut self, client_public: &[u8], proof: &[u8]) -> Result<Vec<u8>> {
        if !matches!(self.state, ServerState::Started) {
            return Err(Error::Pairing("pair-setup-pin fuera de secuencia".into()));
        }
        let group = Group::rfc5054_2048();
        let server = SrpServer::with_variant(
            &group,
            Variant::APPLE_LEGACY,
            self.user.as_bytes(),
            self.pin.as_bytes(),
            &self.salt,
            &self.private_b,
            client_public,
        )?;
        if !server.verify_client_proof(proof) {
            self.state = ServerState::Done;
            return Err(Error::Pairing("código PIN incorrecto".into()));
        }
        let m2 = server.server_proof().to_vec();
        self.state = ServerState::Verified {
            session_key: server.session_key().to_vec(),
        };
        Ok(m2)
    }

    // Step 3: opens the client's sealed Ed25519 key and seals `our_key` for the reply.
    pub fn confirm(&mut self, our_key: &[u8; 32], epk: &[u8], auth_tag: &[u8]) -> Result<PinConfirmed> {
        let ServerState::Verified { session_key } = std::mem::replace(&mut self.state, ServerState::Done) else {
            return Err(Error::Pairing("pair-setup-pin fuera de secuencia".into()));
        };
        if epk.len() != KEY_LEN || auth_tag.len() != TAG_LEN {
            return Err(Error::Pairing("datos de pair-setup-pin con longitud no válida".into()));
        }
        let (key, iv) = gcm_keys(&session_key);
        let gcm = Gcm::new(&key);
        let mut client_key = [0u8; KEY_LEN];
        client_key.copy_from_slice(epk);
        if !gcm.open(&iv, &mut client_key, auth_tag) {
            return Err(Error::Pairing("no se pudo autenticar la clave del cliente".into()));
        }
        let mut sealed = *our_key;
        let tag = gcm.seal(&next_iv(iv), &mut sealed);
        Ok(PinConfirmed {
            client_key,
            epk: sealed,
            auth_tag: tag,
        })
    }
}

// ---------------------------------------------------------------- sender (client) side

pub struct PinSetupClient {
    srp: SrpClient,
    public_a: Vec<u8>,
}

impl PinSetupClient {
    // Consumes the step-1 reply (salt and B).
    pub fn new(user: &str, pin: &str, salt: &[u8], server_public: &[u8]) -> Result<Self> {
        let group = Group::rfc5054_2048();
        let private_a = random::<PRIVATE_LEN>();
        let srp = SrpClient::with_variant(
            &group,
            Variant::APPLE_LEGACY,
            user.as_bytes(),
            pin.as_bytes(),
            salt,
            &private_a,
            server_public,
        )?;
        let public_a = strip_zeros(srp.public_a(&group));
        Ok(Self { srp, public_a })
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public_a
    }

    pub fn proof(&self) -> &[u8] {
        self.srp.proof()
    }

    pub fn verify_server_proof(&self, proof: &[u8]) -> bool {
        self.srp.verify_server_proof(proof)
    }

    // Step 3 request: our Ed25519 key, sealed.
    pub fn seal_key(&self, our_key: &[u8; 32]) -> ([u8; 32], [u8; 16]) {
        let (key, iv) = gcm_keys(self.srp.session_key());
        let mut sealed = *our_key;
        let tag = Gcm::new(&key).seal(&iv, &mut sealed);
        (sealed, tag)
    }

    // Opens the accessory's Ed25519 key from the step-3 reply.
    pub fn open_key(&self, epk: &[u8], auth_tag: &[u8]) -> Result<[u8; 32]> {
        let (key, iv) = gcm_keys(self.srp.session_key());
        let mut out: [u8; KEY_LEN] = epk
            .try_into()
            .map_err(|_| Error::Pairing("clave del accesorio con longitud no válida".into()))?;
        if !Gcm::new(&key).open(&next_iv(iv), &mut out, auth_tag) {
            return Err(Error::Pairing("no se pudo autenticar la clave del accesorio".into()));
        }
        Ok(out)
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

    fn seq(start: u8, len: u8) -> Vec<u8> {
        (start..start + len).collect()
    }

    #[test]
    fn gcm_matches_reference_vectors() {
        // NIST test case 2 (96-bit IV).
        let gcm = Gcm::new(&[0; 16]);
        let mut data = [0u8; 16];
        let tag = gcm.seal(&[0; 12], &mut data);
        assert_eq!(data.to_vec(), unhex("0388dace60b6a392f328c2b971b2fe78"));
        assert_eq!(tag.to_vec(), unhex("ab6e47d42cec13bdf53a67b21257bddf"));
        // 16-byte IV, as AirPlay uses (reference: Python `cryptography`).
        let key: [u8; 16] = seq(0, 16).try_into().unwrap();
        let gcm = Gcm::new(&key);
        let mut data = seq(32, 32);
        let tag = gcm.seal(&seq(16, 16), &mut data);
        assert_eq!(data, unhex("e5bfd595599702affe31e9a98d0201f8c5cc12ac593391cf91bfbdad7f7e4ddf"));
        assert_eq!(tag.to_vec(), unhex("3fd958f0e3832fe2a020cab24a5b7c82"));
        assert!(gcm.open(&seq(16, 16), &mut data, &tag));
        assert_eq!(data, seq(32, 32));
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(!gcm.open(&seq(16, 16), &mut data, &bad));
    }

    #[test]
    fn apple_session_key_is_forty_bytes() {
        let key = super::super::srp::apple_key(super::super::srp::sha1, &[1, 2, 3]);
        assert_eq!(key.len(), 40);
    }

    fn handshake(server_pin: &str, client_pin: &str) -> Result<([u8; 32], [u8; 32])> {
        let user = "AA:BB:CC:DD:EE:FF";
        let mut server = PinSetupServer::start(user, server_pin)?;
        let client = PinSetupClient::new(user, client_pin, server.salt(), server.public_key())?;
        let m2 = server.verify(client.public_key(), client.proof())?;
        assert!(client.verify_server_proof(&m2));
        let (client_key, accessory_key) = ([0x11u8; 32], [0x22u8; 32]);
        let (epk, tag) = client.seal_key(&client_key);
        let confirmed = server.confirm(&accessory_key, &epk, &tag)?;
        assert_eq!(confirmed.client_key, client_key);
        let opened = client.open_key(&confirmed.epk, &confirmed.auth_tag)?;
        Ok((confirmed.client_key, opened))
    }

    #[test]
    fn pin_pairing_round_trip() {
        let (client, accessory) = handshake("4821", "4821").unwrap();
        assert_eq!(client, [0x11; 32]);
        assert_eq!(accessory, [0x22; 32]);
    }

    #[test]
    fn wrong_pin_is_rejected_and_ends_the_exchange() {
        let user = "AA:BB:CC:DD:EE:FF";
        let mut server = PinSetupServer::start(user, "1234").unwrap();
        let client = PinSetupClient::new(user, "4321", server.salt(), server.public_key()).unwrap();
        assert!(server.verify(client.public_key(), client.proof()).is_err());
        let (epk, tag) = client.seal_key(&[1; 32]);
        assert!(server.confirm(&[2; 32], &epk, &tag).is_err());
        assert!(handshake("0000", "0001").is_err());
    }

    #[test]
    fn tampered_key_is_rejected() {
        let user = "dev";
        let mut server = PinSetupServer::start(user, "7777").unwrap();
        let client = PinSetupClient::new(user, "7777", server.salt(), server.public_key()).unwrap();
        server.verify(client.public_key(), client.proof()).unwrap();
        let (mut epk, tag) = client.seal_key(&[3; 32]);
        epk[5] ^= 0x40;
        assert!(server.confirm(&[4; 32], &epk, &tag).is_err());
    }

    #[test]
    fn random_pins_have_four_digits() {
        for _ in 0..50 {
            let pin = random_pin();
            assert_eq!(pin.len(), 4);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
        assert!(PinSetupServer::start("", "1234").is_err());
    }
}
