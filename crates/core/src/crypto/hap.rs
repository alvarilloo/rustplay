use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ctr::Ctr128BE;
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

use super::srp::{self, Group, SrpClient};
use super::stream::hkdf_sha512;
use super::tlv8;
use crate::identity::{Identity, PairingRecord};
use crate::{Error, Result};

const SALT_KEY: &[u8] = b"Pair-Verify-AES-Key";
const SALT_IV: &[u8] = b"Pair-Verify-AES-IV";
const SIG_SIZE: usize = 64;
const KEY_SIZE: usize = 32;
const TRANSIENT_FLAG: u32 = 0x10;

#[derive(Clone)]
pub struct SessionKeys {
    pub shared_secret: [u8; 32],
    // Full key material for HKDF: the 64-byte SRP key after pair-setup, the 32-byte X25519
    // secret after pair-verify.
    pub secret: Vec<u8>,
    pub peer: Option<PairingRecord>,
}

pub enum Step {
    Reply(Vec<u8>),
    Done(SessionKeys),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Initial,
    Setup,
    Handshake,
    Finished,
}

struct Handshake {
    our_x25519: [u8; 32],
    their_x25519: [u8; 32],
    their_ed25519: [u8; 32],
    secret: [u8; 32],
}

pub struct PairServer<'a> {
    pub identity: &'a Identity,
    status: Status,
    handshake: Option<Handshake>,
}

fn derive_16(salt: &[u8], secret: &[u8; 32]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(salt);
    hasher.update(secret);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn verify_ctr(secret: &[u8; 32]) -> Ctr128BE<Aes128> {
    let key = derive_16(SALT_KEY, secret);
    let iv = derive_16(SALT_IV, secret);
    Ctr128BE::<Aes128>::new(&key.into(), &iv.into())
}

fn looks_like_tlv8(body: &[u8]) -> bool {
    body.first()
        .is_some_and(|&t| t == tlv8::STATE || t == tlv8::METHOD)
}

impl<'a> PairServer<'a> {
    pub fn new(identity: &'a Identity) -> Self {
        Self {
            identity,
            status: Status::Initial,
            handshake: None,
        }
    }

    // Legacy pair-setup: the body is the client Ed25519 public key (raw, 32 bytes) and the
    // reply is our raw 32-byte Ed25519 public key.
    pub fn pair_setup(&mut self, body: &[u8]) -> Result<Step> {
        if body.len() != KEY_SIZE {
            if looks_like_tlv8(body) {
                return Err(Error::Pairing("pairing HAP no soportado todavía".into()));
            }
            return Err(Error::Pairing("datos de pair-setup no válidos".into()));
        }
        self.status = Status::Setup;
        Ok(Step::Reply(self.identity.public_key().to_bytes().to_vec()))
    }

    // Legacy pair-verify. Step 1 (`01 00 00 00` + X25519 pk + Ed25519 pk) returns
    // `Reply(our X25519 pk || encrypted signature)`. Step 2 (`00 00 00 00` + encrypted
    // client signature) returns `Done`; the RTSP handler must then answer with an empty 200 OK.
    pub fn pair_verify(&mut self, body: &[u8]) -> Result<Step> {
        let is_legacy = body.len() == 4 + KEY_SIZE + KEY_SIZE || body.len() == 4 + SIG_SIZE;
        if !is_legacy && looks_like_tlv8(body) {
            return Err(Error::Pairing("pairing HAP no soportado todavía".into()));
        }
        match body.first() {
            Some(1) if body.len() == 4 + KEY_SIZE + KEY_SIZE => self.verify_step1(body),
            Some(0) if body.len() == 4 + SIG_SIZE => self.verify_step2(body),
            _ => Err(Error::Pairing("datos de pair-verify no válidos".into())),
        }
    }

    // ECDH shared secret, available after pair-verify step 1 (used by `stream::audio_key`).
    pub fn shared_secret(&self) -> Option<[u8; 32]> {
        self.handshake.as_ref().map(|h| h.secret)
    }

    fn verify_step1(&mut self, body: &[u8]) -> Result<Step> {
        if self.status == Status::Finished {
            return Err(Error::Pairing("pair-verify ya completado".into()));
        }
        let mut their_x25519 = [0u8; 32];
        their_x25519.copy_from_slice(&body[4..36]);
        let mut their_ed25519 = [0u8; 32];
        their_ed25519.copy_from_slice(&body[36..68]);

        let mut seed = [0u8; 32];
        rand::fill(&mut seed[..]);
        let ours = StaticSecret::from(seed);
        let our_x25519 = PublicKey::from(&ours).to_bytes();
        let shared = ours.diffie_hellman(&PublicKey::from(their_x25519));
        if !shared.was_contributory() {
            return Err(Error::Pairing("clave pública X25519 no válida".into()));
        }
        let secret = shared.to_bytes();

        let mut message = [0u8; SIG_SIZE];
        message[..32].copy_from_slice(&our_x25519);
        message[32..].copy_from_slice(&their_x25519);
        let mut signature = self.identity.signing_key.sign(&message).to_bytes();
        verify_ctr(&secret).apply_keystream(&mut signature);

        let mut reply = Vec::with_capacity(KEY_SIZE + SIG_SIZE);
        reply.extend_from_slice(&our_x25519);
        reply.extend_from_slice(&signature);

        self.handshake = Some(Handshake {
            our_x25519,
            their_x25519,
            their_ed25519,
            secret,
        });
        self.status = Status::Handshake;
        Ok(Step::Reply(reply))
    }

    fn verify_step2(&mut self, body: &[u8]) -> Result<Step> {
        let handshake = match (self.status, &self.handshake) {
            (Status::Handshake, Some(h)) => h,
            _ => return Err(Error::Pairing("pair-verify fuera de secuencia".into())),
        };
        // The keystream continues where step 1 left off: skip the 64 bytes used for our signature.
        let mut cipher = verify_ctr(&handshake.secret);
        cipher.apply_keystream(&mut [0u8; SIG_SIZE]);
        let mut signature = [0u8; SIG_SIZE];
        signature.copy_from_slice(&body[4..]);
        cipher.apply_keystream(&mut signature);

        let mut message = [0u8; SIG_SIZE];
        message[..32].copy_from_slice(&handshake.their_x25519);
        message[32..].copy_from_slice(&handshake.our_x25519);
        let key = VerifyingKey::from_bytes(&handshake.their_ed25519)
            .map_err(|_| Error::Pairing("clave Ed25519 del cliente no válida".into()))?;
        key.verify(&message, &Signature::from_bytes(&signature))
            .map_err(|_| Error::Pairing("firma de pair-verify incorrecta".into()))?;

        let keys = SessionKeys {
            shared_secret: handshake.secret,
            secret: handshake.secret.to_vec(),
            peer: None,
        };
        self.status = Status::Finished;
        Ok(Step::Done(keys))
    }
}

enum SetupState {
    Idle,
    AwaitM2 { pin: Option<String> },
    AwaitM4 { srp: SrpClient, transient: bool },
    AwaitM6 { key: Vec<u8> },
    Finished,
}

enum VerifyState {
    Idle,
    AwaitV2 {
        secret: StaticSecret,
        public: [u8; 32],
    },
    AwaitV4 {
        secret: [u8; 32],
    },
    LegacyAwaitV2 {
        secret: StaticSecret,
        public: [u8; 32],
    },
    LegacyAwaitV4 {
        secret: [u8; 32],
    },
    Finished,
}

// Sender (controller) side of HomeKit-style pairing. Each call consumes the last accessory
// reply (`None` on the first call) and returns either the next request body or the result.
pub struct PairClient<'a> {
    pub identity: &'a Identity,
    pub transient_password: String,
    setup: SetupState,
    verify: VerifyState,
}

fn nonce_for(label: &[u8; 8]) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(label);
    nonce.into()
}

fn seal(key: &[u8; 32], label: &[u8; 8], plaintext: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .encrypt(&nonce_for(label), plaintext)
        .map_err(|_| Error::Crypto("fallo al cifrar".into()))
}

fn open(key: &[u8; 32], label: &[u8; 8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    ChaCha20Poly1305::new(key.into())
        .decrypt(&nonce_for(label), ciphertext)
        .map_err(|_| Error::Pairing("no se pudo descifrar la respuesta del accesorio".into()))
}

fn hkdf_32(secret: &[u8], salt: &str, info: &str) -> Result<[u8; 32]> {
    hkdf_sha512(secret, salt.as_bytes(), info.as_bytes())
}

fn parse_reply(reply: Option<&[u8]>, state: u8) -> Result<Vec<(u8, Vec<u8>)>> {
    let reply = reply.ok_or_else(|| Error::Pairing("falta la respuesta del accesorio".into()))?;
    let items = tlv8::decode(reply)?;
    if let Some(code) = tlv8::find(&items, tlv8::ERROR) {
        let code = code.first().copied().unwrap_or(1);
        let name = match code {
            2 => "autenticación (PIN incorrecto)",
            3 => "espera requerida (backoff)",
            4 => "máximo de pares alcanzado",
            5 => "máximo de intentos alcanzado",
            6 => "no disponible",
            7 => "ocupado",
            _ => "desconocido",
        };
        let mut msg = format!("el accesorio rechazó el emparejamiento: {name} ({code})");
        if let Some(delay) = tlv8::find(&items, tlv8::RETRY_DELAY) {
            let secs = delay
                .iter()
                .take(8)
                .enumerate()
                .fold(0u64, |acc, (i, b)| acc | (u64::from(*b) << (8 * i)));
            msg.push_str(&format!(", reintentar en {secs} s"));
        }
        return Err(Error::Pairing(msg));
    }
    match tlv8::find(&items, tlv8::STATE) {
        Some([s]) if *s == state => Ok(items),
        _ => Err(Error::Pairing(format!(
            "respuesta de emparejamiento fuera de secuencia (se esperaba M{state})"
        ))),
    }
}

fn required<'t>(items: &'t [(u8, Vec<u8>)], tag: u8, what: &str) -> Result<&'t [u8]> {
    tlv8::find(items, tag).ok_or_else(|| Error::Pairing(format!("falta {what} en la respuesta")))
}

fn verify_signature(public: &[u8; 32], message: &[u8], signature: &[u8]) -> Result<()> {
    let bytes: [u8; SIG_SIZE] = signature
        .try_into()
        .map_err(|_| Error::Pairing("firma con longitud no válida".into()))?;
    VerifyingKey::from_bytes(public)
        .map_err(|_| Error::Pairing("clave Ed25519 del accesorio no válida".into()))?
        .verify(message, &Signature::from_bytes(&bytes))
        .map_err(|_| Error::Pairing("firma del accesorio incorrecta".into()))
}

fn x25519_pair() -> (StaticSecret, [u8; 32]) {
    let mut seed = [0u8; 32];
    rand::fill(&mut seed[..]);
    let secret = StaticSecret::from(seed);
    let public = PublicKey::from(&secret).to_bytes();
    (secret, public)
}

fn array32(bytes: &[u8], what: &str) -> Result<[u8; 32]> {
    bytes
        .get(..32)
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| Error::Pairing(format!("{what} con longitud no válida")))
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

impl<'a> PairClient<'a> {
    pub fn new(identity: &'a Identity) -> Self {
        Self {
            identity,
            transient_password: "3939".into(),
            setup: SetupState::Idle,
            verify: VerifyState::Idle,
        }
    }

    // `pin == None` is transient pair-setup (M1..M4, SRP password `transient_password`);
    // `Some(pin)` is the full HAP pair-setup (M1..M6) which also exchanges long-term keys.
    pub fn pair_setup(&mut self, pin: Option<&str>, reply: Option<&[u8]>) -> Result<Step> {
        match std::mem::replace(&mut self.setup, SetupState::Finished) {
            SetupState::Idle => {
                let mut out = Vec::new();
                if pin.is_none() {
                    let flags = TRANSIENT_FLAG.to_le_bytes();
                    tlv8::encode(
                        &[
                            (tlv8::METHOD, &[0]),
                            (tlv8::STATE, &[1]),
                            (tlv8::FLAGS, &flags),
                        ],
                        &mut out,
                    );
                } else {
                    tlv8::encode(&[(tlv8::METHOD, &[0]), (tlv8::STATE, &[1])], &mut out);
                }
                self.setup = SetupState::AwaitM2 {
                    pin: pin.map(str::to_owned),
                };
                Ok(Step::Reply(out))
            }
            SetupState::AwaitM2 { pin } => {
                let items = parse_reply(reply, 2)?;
                let salt = required(&items, tlv8::SALT, "la sal SRP")?;
                let server_public = required(&items, tlv8::PUBLIC_KEY, "la clave pública SRP")?;
                let password = pin
                    .clone()
                    .unwrap_or_else(|| self.transient_password.clone());
                let group = Group::rfc5054_3072();
                let mut private = [0u8; 32];
                rand::fill(&mut private[..]);
                let srp = SrpClient::new(
                    &group,
                    srp::sha512,
                    b"Pair-Setup",
                    password.as_bytes(),
                    salt,
                    &private,
                    server_public,
                )?;
                let mut out = Vec::new();
                tlv8::encode(
                    &[
                        (tlv8::STATE, &[3]),
                        (tlv8::PUBLIC_KEY, &srp.public_a(&group)),
                        (tlv8::PROOF, srp.proof()),
                    ],
                    &mut out,
                );
                self.setup = SetupState::AwaitM4 {
                    srp,
                    transient: pin.is_none(),
                };
                Ok(Step::Reply(out))
            }
            SetupState::AwaitM4 { srp, transient } => {
                let items = parse_reply(reply, 4)?;
                let proof = required(&items, tlv8::PROOF, "la prueba del servidor")?;
                if !srp.verify_server_proof(proof) {
                    return Err(Error::Pairing(
                        "la prueba SRP del servidor no coincide".into(),
                    ));
                }
                let key = srp.session_key().to_vec();
                let mut shared_secret = [0u8; 32];
                shared_secret.copy_from_slice(&key[..32]);
                if transient {
                    return Ok(Step::Done(SessionKeys {
                        shared_secret,
                        secret: key,
                        peer: None,
                    }));
                }
                let session_key =
                    hkdf_32(&key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")?;
                let sign_key = hkdf_32(
                    &key,
                    "Pair-Setup-Controller-Sign-Salt",
                    "Pair-Setup-Controller-Sign-Info",
                )?;
                let id = self.identity.pairing_id.as_bytes();
                let ltpk = self.identity.public_key().to_bytes();
                let signature = self
                    .identity
                    .signing_key
                    .sign(&concat(&[&sign_key, id, &ltpk]))
                    .to_bytes();
                let mut sub = Vec::new();
                tlv8::encode(
                    &[
                        (tlv8::IDENTIFIER, id),
                        (tlv8::PUBLIC_KEY, &ltpk),
                        (tlv8::SIGNATURE, &signature),
                    ],
                    &mut sub,
                );
                let encrypted = seal(&session_key, b"PS-Msg05", &sub)?;
                let mut out = Vec::new();
                tlv8::encode(
                    &[(tlv8::ENCRYPTED_DATA, &encrypted), (tlv8::STATE, &[5])],
                    &mut out,
                );
                self.setup = SetupState::AwaitM6 { key };
                Ok(Step::Reply(out))
            }
            SetupState::AwaitM6 { key } => {
                let items = parse_reply(reply, 6)?;
                let encrypted = required(&items, tlv8::ENCRYPTED_DATA, "los datos cifrados M6")?;
                let session_key =
                    hkdf_32(&key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")?;
                let plain = open(&session_key, b"PS-Msg06", encrypted)?;
                let sub = tlv8::decode(&plain)?;
                let id = required(&sub, tlv8::IDENTIFIER, "el identificador del accesorio")?;
                let ltpk = array32(
                    required(&sub, tlv8::PUBLIC_KEY, "la clave del accesorio")?,
                    "clave del accesorio",
                )?;
                let signature = required(&sub, tlv8::SIGNATURE, "la firma del accesorio")?;
                let accessory_x = hkdf_32(
                    &key,
                    "Pair-Setup-Accessory-Sign-Salt",
                    "Pair-Setup-Accessory-Sign-Info",
                )?;
                verify_signature(&ltpk, &concat(&[&accessory_x, id, &ltpk]), signature)?;
                let mut shared_secret = [0u8; 32];
                shared_secret.copy_from_slice(&key[..32]);
                Ok(Step::Done(SessionKeys {
                    shared_secret,
                    secret: key,
                    peer: Some(PairingRecord {
                        peer_id: String::from_utf8_lossy(id).into_owned(),
                        peer_public_key: ltpk,
                    }),
                }))
            }
            SetupState::Finished => Err(Error::Pairing("pair-setup ya completado".into())),
        }
    }

    // HAP pair-verify. With `peer_public_key` the accessory signature is checked; `None`
    // (transient or unknown accessory) skips that check.
    pub fn pair_verify(
        &mut self,
        peer_public_key: Option<&[u8; 32]>,
        reply: Option<&[u8]>,
    ) -> Result<Step> {
        match std::mem::replace(&mut self.verify, VerifyState::Finished) {
            VerifyState::Idle => {
                let (secret, public) = x25519_pair();
                let mut out = Vec::new();
                tlv8::encode(
                    &[(tlv8::STATE, &[1]), (tlv8::PUBLIC_KEY, &public)],
                    &mut out,
                );
                self.verify = VerifyState::AwaitV2 { secret, public };
                Ok(Step::Reply(out))
            }
            VerifyState::AwaitV2 { secret, public } => {
                let items = parse_reply(reply, 2)?;
                let server_public = array32(
                    required(&items, tlv8::PUBLIC_KEY, "la clave X25519")?,
                    "clave X25519",
                )?;
                let shared = secret.diffie_hellman(&PublicKey::from(server_public));
                if !shared.was_contributory() {
                    return Err(Error::Pairing("clave pública X25519 no válida".into()));
                }
                let shared = shared.to_bytes();
                let verify_key = hkdf_32(
                    &shared,
                    "Pair-Verify-Encrypt-Salt",
                    "Pair-Verify-Encrypt-Info",
                )?;
                match (tlv8::find(&items, tlv8::ENCRYPTED_DATA), peer_public_key) {
                    (Some(encrypted), peer) => {
                        let plain = open(&verify_key, b"PV-Msg02", encrypted)?;
                        if let Some(peer) = peer {
                            let sub = tlv8::decode(&plain)?;
                            let id =
                                required(&sub, tlv8::IDENTIFIER, "el identificador del accesorio")?;
                            let signature =
                                required(&sub, tlv8::SIGNATURE, "la firma del accesorio")?;
                            verify_signature(
                                peer,
                                &concat(&[&server_public, id, &public]),
                                signature,
                            )?;
                        }
                    }
                    (None, Some(_)) => {
                        return Err(Error::Pairing(
                            "falta la prueba cifrada del accesorio".into(),
                        ));
                    }
                    (None, None) => {}
                }

                let id = self.identity.pairing_id.as_bytes();
                let signature = self
                    .identity
                    .signing_key
                    .sign(&concat(&[&public, id, &server_public]))
                    .to_bytes();
                let mut sub = Vec::new();
                tlv8::encode(
                    &[(tlv8::IDENTIFIER, id), (tlv8::SIGNATURE, &signature)],
                    &mut sub,
                );
                let encrypted = seal(&verify_key, b"PV-Msg03", &sub)?;
                let mut out = Vec::new();
                tlv8::encode(
                    &[(tlv8::STATE, &[3]), (tlv8::ENCRYPTED_DATA, &encrypted)],
                    &mut out,
                );
                self.verify = VerifyState::AwaitV4 { secret: shared };
                Ok(Step::Reply(out))
            }
            VerifyState::AwaitV4 { secret } => {
                if let Some(body) = reply.filter(|b| !b.is_empty()) {
                    let items = tlv8::decode(body)?;
                    if tlv8::find(&items, tlv8::ERROR).is_some() {
                        parse_reply(Some(body), 4)?;
                    }
                }
                Ok(Step::Done(SessionKeys {
                    shared_secret: secret,
                    secret: secret.to_vec(),
                    peer: None,
                }))
            }
            _ => Err(Error::Pairing("pair-verify fuera de secuencia".into())),
        }
    }

    // Legacy (non-TLV) pair-setup: POST our raw Ed25519 public key; the reply is the accessory's.
    pub fn pair_setup_legacy(&mut self, reply: Option<&[u8]>) -> Result<Step> {
        match reply {
            None => Ok(Step::Reply(self.identity.public_key().to_bytes().to_vec())),
            Some(reply) => {
                if reply.len() != KEY_SIZE {
                    return Err(Error::Pairing("respuesta de pair-setup no válida".into()));
                }
                Ok(Step::Done(SessionKeys {
                    shared_secret: [0; 32],
                    secret: Vec::new(),
                    peer: Some(PairingRecord {
                        peer_id: String::new(),
                        peer_public_key: array32(reply, "clave del accesorio")?,
                    }),
                }))
            }
        }
    }

    // Legacy raw pair-verify (see `PairServer::pair_verify`); `server_public_key` is the
    // accessory Ed25519 key (`pk` from /info or the legacy pair-setup reply). The channel stays
    // in plaintext afterwards.
    pub fn pair_verify_legacy(
        &mut self,
        server_public_key: &[u8; 32],
        reply: Option<&[u8]>,
    ) -> Result<Step> {
        match std::mem::replace(&mut self.verify, VerifyState::Finished) {
            VerifyState::Idle => {
                let (secret, public) = x25519_pair();
                let mut out = Vec::with_capacity(4 + KEY_SIZE + KEY_SIZE);
                out.extend_from_slice(&[1, 0, 0, 0]);
                out.extend_from_slice(&public);
                out.extend_from_slice(&self.identity.public_key().to_bytes());
                self.verify = VerifyState::LegacyAwaitV2 { secret, public };
                Ok(Step::Reply(out))
            }
            VerifyState::LegacyAwaitV2 { secret, public } => {
                let reply = reply
                    .filter(|r| r.len() == KEY_SIZE + SIG_SIZE)
                    .ok_or_else(|| Error::Pairing("respuesta V2 no válida".into()))?;
                let server_x = array32(&reply[..32], "clave X25519")?;
                let shared = secret.diffie_hellman(&PublicKey::from(server_x));
                if !shared.was_contributory() {
                    return Err(Error::Pairing("clave pública X25519 no válida".into()));
                }
                let shared = shared.to_bytes();
                let mut cipher = verify_ctr(&shared);
                let mut server_sig = [0u8; SIG_SIZE];
                server_sig.copy_from_slice(&reply[32..]);
                cipher.apply_keystream(&mut server_sig);
                verify_signature(
                    server_public_key,
                    &concat(&[&server_x, &public]),
                    &server_sig,
                )?;

                let mut signature = self
                    .identity
                    .signing_key
                    .sign(&concat(&[&public, &server_x]))
                    .to_bytes();
                cipher.apply_keystream(&mut signature);
                let mut out = vec![0, 0, 0, 0];
                out.extend_from_slice(&signature);
                self.verify = VerifyState::LegacyAwaitV4 { secret: shared };
                Ok(Step::Reply(out))
            }
            VerifyState::LegacyAwaitV4 { secret } => Ok(Step::Done(SessionKeys {
                shared_secret: secret,
                secret: secret.to_vec(),
                peer: None,
            })),
            _ => Err(Error::Pairing("pair-verify fuera de secuencia".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    fn identity() -> Identity {
        Identity {
            device_id: [1, 2, 3, 4, 5, 6],
            signing_key: SigningKey::from_bytes(&[7u8; 32]),
            pairing_id: "TEST".into(),
        }
    }

    struct Client {
        signing: SigningKey,
        x_secret: StaticSecret,
        x_public: [u8; 32],
    }

    impl Client {
        fn new() -> Self {
            let mut seed = [0u8; 32];
            rand::fill(&mut seed[..]);
            let x_secret = StaticSecret::from(seed);
            rand::fill(&mut seed[..]);
            Self {
                signing: SigningKey::from_bytes(&seed),
                x_public: PublicKey::from(&x_secret).to_bytes(),
                x_secret,
            }
        }

        fn step1(&self) -> Vec<u8> {
            let mut body = vec![1, 0, 0, 0];
            body.extend_from_slice(&self.x_public);
            body.extend_from_slice(&self.signing.verifying_key().to_bytes());
            body
        }
    }

    fn reply(step: Step) -> Vec<u8> {
        match step {
            Step::Reply(r) => r,
            Step::Done(_) => panic!("se esperaba una respuesta"),
        }
    }

    #[test]
    fn pair_setup_returns_our_public_key() {
        let identity = identity();
        let mut server = PairServer::new(&identity);
        let out = reply(server.pair_setup(&[9u8; 32]).unwrap());
        assert_eq!(out, identity.public_key().to_bytes());
        assert!(server.pair_setup(&[9u8; 31]).is_err());
    }

    #[test]
    fn hap_tlv8_variant_is_reported_as_unsupported() {
        let identity = identity();
        let mut server = PairServer::new(&identity);
        let setup = [0x06, 0x01, 0x01, 0x00, 0x01, 0x00];
        let verify = [0x06, 0x01, 0x01, 0x03, 0x20, 1, 2, 3];
        for result in [server.pair_setup(&setup), server.pair_verify(&verify)] {
            match result {
                Err(Error::Pairing(msg)) => assert_eq!(msg, "pairing HAP no soportado todavía"),
                _ => panic!("se esperaba Error::Pairing"),
            }
        }
    }

    #[test]
    fn verify_ctr_keystream_matches_reference() {
        let secret: [u8; 32] = std::array::from_fn(|i| i as u8);
        let mut stream = [0u8; 80];
        verify_ctr(&secret).apply_keystream(&mut stream);
        let expected = "cee447115c4131af68db0486d99d7972b530389cff1bd79c5ef4772e60f765150f7ba6f703a45fb1dadc107d107fbaa2c36a1e8d433ccc72aa212658c72ac8fe1ce52e1bfb54d8e5ce173f7eb0e58754";
        let hex: String = stream.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn full_legacy_handshake_with_simulated_client() {
        let identity = identity();
        let mut server = PairServer::new(&identity);
        let client = Client::new();
        server
            .pair_setup(&client.signing.verifying_key().to_bytes())
            .unwrap();

        let r1 = reply(server.pair_verify(&client.step1()).unwrap());
        assert_eq!(r1.len(), 96);
        let server_x: [u8; 32] = r1[..32].try_into().unwrap();
        let secret = client
            .x_secret
            .diffie_hellman(&PublicKey::from(server_x))
            .to_bytes();
        assert_eq!(server.shared_secret(), Some(secret));

        let mut cipher = verify_ctr(&secret);
        let mut server_sig: [u8; 64] = r1[32..].try_into().unwrap();
        cipher.apply_keystream(&mut server_sig);
        let mut message = [0u8; 64];
        message[..32].copy_from_slice(&server_x);
        message[32..].copy_from_slice(&client.x_public);
        identity
            .public_key()
            .verify(&message, &Signature::from_bytes(&server_sig))
            .expect("la firma del servidor debe verificar");

        // The client keeps using the same keystream: its signature is encrypted at offset 64.
        message[..32].copy_from_slice(&client.x_public);
        message[32..].copy_from_slice(&server_x);
        let mut client_sig = client.signing.sign(&message).to_bytes();
        cipher.apply_keystream(&mut client_sig);
        let mut body = vec![0, 0, 0, 0];
        body.extend_from_slice(&client_sig);
        match server.pair_verify(&body).unwrap() {
            Step::Done(keys) => assert_eq!(keys.shared_secret, secret),
            Step::Reply(_) => panic!("se esperaba Done"),
        }
        assert!(server.pair_verify(&client.step1()).is_err());
    }

    #[test]
    fn bad_signature_or_wrong_order_is_rejected() {
        let identity = identity();
        let mut server = PairServer::new(&identity);
        let client = Client::new();
        let mut body = vec![0, 0, 0, 0];
        body.extend_from_slice(&[0u8; 64]);
        assert!(server.pair_verify(&body).is_err());
        server.pair_verify(&client.step1()).unwrap();
        assert!(server.pair_verify(&body).is_err());
        assert!(server.pair_verify(&[2, 0, 0, 0]).is_err());
    }

    type VerifyKeys = (StaticSecret, [u8; 32], [u8; 32], [u8; 32]);

    // Simulated HAP accessory (pair-setup and pair-verify servers).
    struct Accessory {
        ltsk: SigningKey,
        id: &'static [u8],
        password: &'static [u8],
        srp: Option<srp::SrpServer>,
        group: Group,
        client_public: Vec<u8>,
        key: Vec<u8>,
        controller_ltpk: Option<(Vec<u8>, [u8; 32])>,
        verify_secret: Option<VerifyKeys>,
        saw_transient_flag: bool,
    }

    fn tlv(items: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        tlv8::encode(items, &mut out);
        out
    }

    impl Accessory {
        fn new(password: &'static [u8]) -> Self {
            Self {
                ltsk: SigningKey::from_bytes(&[42u8; 32]),
                id: b"ACCESSORY-ID",
                password,
                srp: None,
                group: Group::rfc5054_3072(),
                client_public: Vec::new(),
                key: Vec::new(),
                controller_ltpk: None,
                verify_secret: None,
                saw_transient_flag: false,
            }
        }

        fn m2(&mut self, m1: &[u8]) -> Vec<u8> {
            let items = tlv8::decode(m1).unwrap();
            assert_eq!(tlv8::find(&items, tlv8::STATE).unwrap(), [1]);
            assert_eq!(tlv8::find(&items, tlv8::METHOD).unwrap(), [0]);
            self.saw_transient_flag = tlv8::find(&items, tlv8::FLAGS) == Some(&[0x10, 0, 0, 0][..]);
            let salt = [0x55u8; 16];
            let probe = srp::SrpServer::new(
                &self.group,
                srp::sha512,
                b"Pair-Setup",
                self.password,
                &salt,
                &[0x77u8; 32],
                &[1u8],
            )
            .unwrap();
            let public_b = probe.public_b(&self.group);
            self.srp = Some(probe);
            tlv(&[
                (tlv8::STATE, &[2]),
                (tlv8::PUBLIC_KEY, &public_b),
                (tlv8::SALT, &salt),
            ])
        }

        fn m4(&mut self, m3: &[u8]) -> Vec<u8> {
            let items = tlv8::decode(m3).unwrap();
            assert_eq!(tlv8::find(&items, tlv8::STATE).unwrap(), [3]);
            let a = tlv8::find(&items, tlv8::PUBLIC_KEY).unwrap();
            assert_eq!(a.len(), 384);
            let server = srp::SrpServer::new(
                &self.group,
                srp::sha512,
                b"Pair-Setup",
                self.password,
                &[0x55u8; 16],
                &[0x77u8; 32],
                a,
            )
            .unwrap();
            if !server.verify_client_proof(tlv8::find(&items, tlv8::PROOF).unwrap()) {
                return tlv(&[(tlv8::STATE, &[4]), (tlv8::ERROR, &[2])]);
            }
            self.key = server.session_key().to_vec();
            let proof = server.server_proof().to_vec();
            self.client_public = a.to_vec();
            self.srp = Some(server);
            tlv(&[(tlv8::STATE, &[4]), (tlv8::PROOF, &proof)])
        }

        fn m6(&mut self, m5: &[u8]) -> Vec<u8> {
            let items = tlv8::decode(m5).unwrap();
            assert_eq!(tlv8::find(&items, tlv8::STATE).unwrap(), [5]);
            let session_key = hkdf_32(
                &self.key,
                "Pair-Setup-Encrypt-Salt",
                "Pair-Setup-Encrypt-Info",
            )
            .unwrap();
            let plain = open(
                &session_key,
                b"PS-Msg05",
                tlv8::find(&items, tlv8::ENCRYPTED_DATA).unwrap(),
            )
            .unwrap();
            let sub = tlv8::decode(&plain).unwrap();
            let id = tlv8::find(&sub, tlv8::IDENTIFIER).unwrap().to_vec();
            let ltpk: [u8; 32] = tlv8::find(&sub, tlv8::PUBLIC_KEY)
                .unwrap()
                .try_into()
                .unwrap();
            let sign_key = hkdf_32(
                &self.key,
                "Pair-Setup-Controller-Sign-Salt",
                "Pair-Setup-Controller-Sign-Info",
            )
            .unwrap();
            verify_signature(
                &ltpk,
                &concat(&[&sign_key, &id, &ltpk]),
                tlv8::find(&sub, tlv8::SIGNATURE).unwrap(),
            )
            .unwrap();
            self.controller_ltpk = Some((id, ltpk));

            let accessory_x = hkdf_32(
                &self.key,
                "Pair-Setup-Accessory-Sign-Salt",
                "Pair-Setup-Accessory-Sign-Info",
            )
            .unwrap();
            let ltpk = self.ltsk.verifying_key().to_bytes();
            let signature = self
                .ltsk
                .sign(&concat(&[&accessory_x, self.id, &ltpk]))
                .to_bytes();
            let sub = tlv(&[
                (tlv8::IDENTIFIER, self.id),
                (tlv8::PUBLIC_KEY, &ltpk),
                (tlv8::SIGNATURE, &signature),
            ]);
            let encrypted = seal(&session_key, b"PS-Msg06", &sub).unwrap();
            tlv(&[(tlv8::STATE, &[6]), (tlv8::ENCRYPTED_DATA, &encrypted)])
        }

        fn v2(&mut self, v1: &[u8]) -> Vec<u8> {
            let items = tlv8::decode(v1).unwrap();
            assert_eq!(tlv8::find(&items, tlv8::STATE).unwrap(), [1]);
            let client_public: [u8; 32] = tlv8::find(&items, tlv8::PUBLIC_KEY)
                .unwrap()
                .try_into()
                .unwrap();
            let (secret, public) = x25519_pair();
            let shared = secret
                .diffie_hellman(&PublicKey::from(client_public))
                .to_bytes();
            let key = hkdf_32(
                &shared,
                "Pair-Verify-Encrypt-Salt",
                "Pair-Verify-Encrypt-Info",
            )
            .unwrap();
            let signature = self
                .ltsk
                .sign(&concat(&[&public, self.id, &client_public]))
                .to_bytes();
            let sub = tlv(&[(tlv8::IDENTIFIER, self.id), (tlv8::SIGNATURE, &signature)]);
            let encrypted = seal(&key, b"PV-Msg02", &sub).unwrap();
            self.verify_secret = Some((secret, public, client_public, shared));
            tlv(&[
                (tlv8::STATE, &[2]),
                (tlv8::PUBLIC_KEY, &public),
                (tlv8::ENCRYPTED_DATA, &encrypted),
            ])
        }

        fn v4(&mut self, v3: &[u8], controller: Option<[u8; 32]>) -> Vec<u8> {
            let (_, public, client_public, shared) = self.verify_secret.take().unwrap();
            let items = tlv8::decode(v3).unwrap();
            assert_eq!(tlv8::find(&items, tlv8::STATE).unwrap(), [3]);
            let key = hkdf_32(
                &shared,
                "Pair-Verify-Encrypt-Salt",
                "Pair-Verify-Encrypt-Info",
            )
            .unwrap();
            let plain = open(
                &key,
                b"PV-Msg03",
                tlv8::find(&items, tlv8::ENCRYPTED_DATA).unwrap(),
            )
            .unwrap();
            let sub = tlv8::decode(&plain).unwrap();
            let id = tlv8::find(&sub, tlv8::IDENTIFIER).unwrap();
            if let Some(ltpk) = controller {
                verify_signature(
                    &ltpk,
                    &concat(&[&client_public, id, &public]),
                    tlv8::find(&sub, tlv8::SIGNATURE).unwrap(),
                )
                .unwrap();
            }
            tlv(&[(tlv8::STATE, &[4])])
        }
    }

    fn drive_setup(
        client: &mut PairClient,
        accessory: &mut Accessory,
        pin: Option<&str>,
    ) -> Result<SessionKeys> {
        let m1 = reply(client.pair_setup(pin, None)?);
        let m2 = accessory.m2(&m1);
        let m3 = reply(client.pair_setup(pin, Some(&m2))?);
        let m4 = accessory.m4(&m3);
        match client.pair_setup(pin, Some(&m4))? {
            Step::Done(keys) => Ok(keys),
            Step::Reply(m5) => {
                let m6 = accessory.m6(&m5);
                match client.pair_setup(pin, Some(&m6))? {
                    Step::Done(keys) => Ok(keys),
                    Step::Reply(_) => panic!("se esperaba Done"),
                }
            }
        }
    }

    #[test]
    fn transient_pair_setup_ends_after_m4() {
        let identity = identity();
        let mut client = PairClient::new(&identity);
        let mut accessory = Accessory::new(b"3939");
        let keys = drive_setup(&mut client, &mut accessory, None).unwrap();
        assert!(accessory.saw_transient_flag);
        assert!(keys.peer.is_none());
        assert_eq!(keys.secret.len(), 64);
        assert_eq!(keys.secret, accessory.key);
        assert_eq!(keys.shared_secret[..], keys.secret[..32]);
        assert!(accessory.controller_ltpk.is_none());
    }

    #[test]
    fn transient_pair_setup_fails_with_wrong_password() {
        let identity = identity();
        let mut client = PairClient::new(&identity);
        let mut accessory = Accessory::new(b"0000");
        match drive_setup(&mut client, &mut accessory, None) {
            Err(Error::Pairing(msg)) => assert!(msg.contains("autenticación"), "{msg}"),
            _ => panic!("se esperaba un error de emparejamiento"),
        }
    }

    #[test]
    fn pin_pair_setup_and_verify_round_trip() {
        let identity = identity();
        let mut accessory = Accessory::new(b"1234");
        let mut client = PairClient::new(&identity);
        let keys = drive_setup(&mut client, &mut accessory, Some("1234")).unwrap();
        assert!(!accessory.saw_transient_flag);
        let peer = keys.peer.expect("registro del accesorio");
        assert_eq!(peer.peer_id, "ACCESSORY-ID");
        assert_eq!(
            peer.peer_public_key,
            accessory.ltsk.verifying_key().to_bytes()
        );
        let (id, ltpk) = accessory.controller_ltpk.clone().unwrap();
        assert_eq!(id, identity.pairing_id.as_bytes());
        assert_eq!(ltpk, identity.public_key().to_bytes());

        let mut client = PairClient::new(&identity);
        let v1 = reply(
            client
                .pair_verify(Some(&peer.peer_public_key), None)
                .unwrap(),
        );
        let v2 = accessory.v2(&v1);
        let v3 = reply(
            client
                .pair_verify(Some(&peer.peer_public_key), Some(&v2))
                .unwrap(),
        );
        let shared = accessory.verify_secret.as_ref().unwrap().3;
        let v4 = accessory.v4(&v3, Some(ltpk));
        match client
            .pair_verify(Some(&peer.peer_public_key), Some(&v4))
            .unwrap()
        {
            Step::Done(keys) => {
                assert_eq!(keys.shared_secret, shared);
                assert!(keys.peer.is_none());
                let (write, read) =
                    super::super::stream::control_keys(&keys.shared_secret).unwrap();
                assert_ne!(write, read);
            }
            Step::Reply(_) => panic!("se esperaba Done"),
        }
    }

    #[test]
    fn pair_verify_rejects_wrong_accessory_key_and_skips_when_none() {
        let identity = identity();
        let mut accessory = Accessory::new(b"1234");
        let wrong = SigningKey::from_bytes(&[1u8; 32])
            .verifying_key()
            .to_bytes();
        let mut client = PairClient::new(&identity);
        let v1 = reply(client.pair_verify(Some(&wrong), None).unwrap());
        let v2 = accessory.v2(&v1);
        assert!(client.pair_verify(Some(&wrong), Some(&v2)).is_err());

        let mut client = PairClient::new(&identity);
        let v1 = reply(client.pair_verify(None, None).unwrap());
        let v2 = accessory.v2(&v1);
        let v3 = reply(client.pair_verify(None, Some(&v2)).unwrap());
        let v4 = accessory.v4(&v3, None);
        assert!(matches!(
            client.pair_verify(None, Some(&v4)).unwrap(),
            Step::Done(_)
        ));
    }

    #[test]
    fn setup_reports_accessory_errors_and_sequence_problems() {
        let identity = identity();
        let mut client = PairClient::new(&identity);
        reply(client.pair_setup(None, None).unwrap());
        let backoff = tlv(&[
            (tlv8::STATE, &[2]),
            (tlv8::ERROR, &[3]),
            (tlv8::RETRY_DELAY, &[5]),
        ]);
        match client.pair_setup(None, Some(&backoff)) {
            Err(Error::Pairing(msg)) => assert!(msg.contains("backoff") && msg.contains("5 s")),
            _ => panic!("se esperaba un error"),
        }
        assert!(client.pair_setup(None, None).is_err());

        let mut client = PairClient::new(&identity);
        reply(client.pair_setup(Some("1234"), None).unwrap());
        let wrong_state = tlv(&[(tlv8::STATE, &[4])]);
        assert!(client.pair_setup(Some("1234"), Some(&wrong_state)).is_err());
    }

    #[test]
    fn legacy_pair_verify_round_trip_with_pair_server() {
        let server_identity = identity();
        let mut client_identity = identity();
        client_identity.signing_key = SigningKey::from_bytes(&[8u8; 32]);
        let mut server = PairServer::new(&server_identity);
        let mut client = PairClient::new(&client_identity);
        let setup = reply(client.pair_setup_legacy(None).unwrap());
        let server_pk = reply(server.pair_setup(&setup).unwrap());
        let record = match client.pair_setup_legacy(Some(&server_pk)).unwrap() {
            Step::Done(keys) => keys.peer.unwrap(),
            Step::Reply(_) => panic!("se esperaba Done"),
        };
        let v1 = reply(
            client
                .pair_verify_legacy(&record.peer_public_key, None)
                .unwrap(),
        );
        let v2 = reply(server.pair_verify(&v1).unwrap());
        let v3 = reply(
            client
                .pair_verify_legacy(&record.peer_public_key, Some(&v2))
                .unwrap(),
        );
        let secret = match server.pair_verify(&v3).unwrap() {
            Step::Done(keys) => keys.shared_secret,
            Step::Reply(_) => panic!("se esperaba Done"),
        };
        match client
            .pair_verify_legacy(&record.peer_public_key, Some(&[]))
            .unwrap()
        {
            Step::Done(keys) => assert_eq!(keys.shared_secret, secret),
            Step::Reply(_) => panic!("se esperaba Done"),
        }
    }
}
