use num_bigint::BigUint;
use sha2::{Digest, Sha512};

use crate::{Error, Result};

const N_3072_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);

const N_2048_HEX: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4",
    "A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF60",
    "95179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF",
    "747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B907",
    "8717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861",
    "60279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DB",
    "FBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73",
);

pub type HashFn = fn(&[&[u8]]) -> Vec<u8>;
// Derives the session key K from the shared secret S (big-endian, unpadded).
pub type KeyFn = fn(HashFn, &[u8]) -> Vec<u8>;

// K = H(S), as in RFC 5054 and HomeKit.
pub fn plain_key(hash: HashFn, secret: &[u8]) -> Vec<u8> {
    hash(&[secret])
}

// Apple's legacy AirPlay variant: K = H(S | 00000000) | H(S | 00000001), twice the hash size.
pub fn apple_key(hash: HashFn, secret: &[u8]) -> Vec<u8> {
    let mut key = hash(&[secret, &[0, 0, 0, 0]]);
    key.extend_from_slice(&hash(&[secret, &[0, 0, 0, 1]]));
    key
}

#[derive(Clone, Copy)]
pub struct Variant {
    pub hash: HashFn,
    pub key: KeyFn,
}

impl Variant {
    // Legacy AirPlay PIN pairing (/pair-setup-pin): SHA-1 with the doubled session key.
    pub const APPLE_LEGACY: Variant = Variant { hash: sha1, key: apple_key };
}

pub fn sha1(parts: &[&[u8]]) -> Vec<u8> {
    use sha1::Digest as _;
    let mut hasher = sha1::Sha1::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

pub fn sha512(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

pub struct Group {
    pub n: BigUint,
    pub g: BigUint,
    pub width: usize,
}

impl Group {
    pub fn rfc5054_3072() -> Self {
        Self {
            n: BigUint::parse_bytes(N_3072_HEX.as_bytes(), 16).expect("constante N válida"),
            g: BigUint::from(5u8),
            width: 384,
        }
    }

    pub fn rfc5054_2048() -> Self {
        Self {
            n: BigUint::parse_bytes(N_2048_HEX.as_bytes(), 16).expect("constante N válida"),
            g: BigUint::from(2u8),
            width: 256,
        }
    }

    pub fn pad(&self, value: &BigUint) -> Vec<u8> {
        let bytes = value.to_bytes_be();
        let mut out = vec![0u8; self.width.saturating_sub(bytes.len())];
        out.extend_from_slice(&bytes);
        out
    }
}

fn is_zero_mod(value: &BigUint, n: &BigUint) -> bool {
    (value % n) == BigUint::from(0u8)
}

fn xor_hashes(hash: HashFn, n: &BigUint, g: &BigUint) -> Vec<u8> {
    let hn = hash(&[&n.to_bytes_be()]);
    let hg = hash(&[&g.to_bytes_be()]);
    hn.iter().zip(&hg).map(|(a, b)| a ^ b).collect()
}

// SRP-6a client as used by HomeKit/AirPlay pair-setup: x = H(s | H(I ":" P)), k = H(N | PAD(g)),
// u = H(PAD(A) | PAD(B)), K = H(S) and M1 = H(H(N) xor H(g) | H(I) | s | A | B | K) with A and B
// in their natural (unpadded) byte form; M2 = H(A | M1 | K).
pub struct SrpClient {
    public_a: BigUint,
    session_key: Vec<u8>,
    proof: Vec<u8>,
    server_proof: Vec<u8>,
}

impl SrpClient {
    pub fn new(
        group: &Group,
        hash: HashFn,
        username: &[u8],
        password: &[u8],
        salt: &[u8],
        private_a: &[u8],
        server_public: &[u8],
    ) -> Result<Self> {
        let variant = Variant { hash, key: plain_key };
        Self::with_variant(group, variant, username, password, salt, private_a, server_public)
    }

    pub fn with_variant(
        group: &Group,
        variant: Variant,
        username: &[u8],
        password: &[u8],
        salt: &[u8],
        private_a: &[u8],
        server_public: &[u8],
    ) -> Result<Self> {
        let hash = variant.hash;
        let n = &group.n;
        let g = &group.g;
        let b_pub = BigUint::from_bytes_be(server_public);
        if is_zero_mod(&b_pub, n) {
            return Err(Error::Pairing(
                "clave pública SRP del servidor no válida".into(),
            ));
        }
        let mut a = BigUint::from_bytes_be(private_a);
        if is_zero_mod(&a, n) {
            a = BigUint::from(1u8);
        }

        let inner = hash(&[username, b":", password]);
        let x = BigUint::from_bytes_be(&hash(&[salt, &inner]));
        let k = BigUint::from_bytes_be(&hash(&[&group.pad(n), &group.pad(g)]));
        let public_a = g.modpow(&a, n);
        let u = BigUint::from_bytes_be(&hash(&[&group.pad(&public_a), &group.pad(&b_pub)]));
        if is_zero_mod(&u, n) {
            return Err(Error::Pairing("parámetro SRP u no válido".into()));
        }

        let kgx = (&k * g.modpow(&x, n)) % n;
        let base = ((&b_pub % n) + n - kgx) % n;
        let exponent = &a + &u * &x;
        let s = base.modpow(&exponent, n);
        let session_key = (variant.key)(hash, &s.to_bytes_be());

        let hxor = xor_hashes(hash, n, g);
        let hu = hash(&[username]);
        let a_bytes = public_a.to_bytes_be();
        let b_bytes = b_pub.to_bytes_be();
        let proof = hash(&[&hxor, &hu, salt, &a_bytes, &b_bytes, &session_key]);
        let server_proof = hash(&[&a_bytes, &proof, &session_key]);

        Ok(Self {
            public_a,
            session_key,
            proof,
            server_proof,
        })
    }

    pub fn public_a(&self, group: &Group) -> Vec<u8> {
        group.pad(&self.public_a)
    }

    pub fn proof(&self) -> &[u8] {
        &self.proof
    }

    pub fn session_key(&self) -> &[u8] {
        &self.session_key
    }

    pub fn verify_server_proof(&self, proof: &[u8]) -> bool {
        proof == self.server_proof.as_slice()
    }
}

// Accessory side: simulated accessories in tests and the receiver's PIN pairing.
pub struct SrpServer {
    public_b: BigUint,
    session_key: Vec<u8>,
    expected_proof: Vec<u8>,
    server_proof: Vec<u8>,
}

impl SrpServer {
    pub fn new(
        group: &Group,
        hash: HashFn,
        username: &[u8],
        password: &[u8],
        salt: &[u8],
        private_b: &[u8],
        client_public: &[u8],
    ) -> Result<Self> {
        let variant = Variant { hash, key: plain_key };
        Self::with_variant(group, variant, username, password, salt, private_b, client_public)
    }

    pub fn with_variant(
        group: &Group,
        variant: Variant,
        username: &[u8],
        password: &[u8],
        salt: &[u8],
        private_b: &[u8],
        client_public: &[u8],
    ) -> Result<Self> {
        let hash = variant.hash;
        let n = &group.n;
        let g = &group.g;
        let a_pub = BigUint::from_bytes_be(client_public);
        if is_zero_mod(&a_pub, n) {
            return Err(Error::Pairing(
                "clave pública SRP del cliente no válida".into(),
            ));
        }
        let b = BigUint::from_bytes_be(private_b);
        let inner = hash(&[username, b":", password]);
        let x = BigUint::from_bytes_be(&hash(&[salt, &inner]));
        let k = BigUint::from_bytes_be(&hash(&[&group.pad(n), &group.pad(g)]));
        let v = g.modpow(&x, n);
        let public_b = (&k * &v + g.modpow(&b, n)) % n;
        let u = BigUint::from_bytes_be(&hash(&[&group.pad(&a_pub), &group.pad(&public_b)]));
        let s = ((&a_pub * v.modpow(&u, n)) % n).modpow(&b, n);
        let session_key = (variant.key)(hash, &s.to_bytes_be());

        let hxor = xor_hashes(hash, n, g);
        let hu = hash(&[username]);
        let a_bytes = a_pub.to_bytes_be();
        let expected_proof = hash(&[
            &hxor,
            &hu,
            salt,
            &a_bytes,
            &public_b.to_bytes_be(),
            &session_key,
        ]);
        let server_proof = hash(&[&a_bytes, &expected_proof, &session_key]);
        Ok(Self {
            public_b,
            session_key,
            expected_proof,
            server_proof,
        })
    }

    pub fn public_b(&self, group: &Group) -> Vec<u8> {
        group.pad(&self.public_b)
    }

    pub fn verify_client_proof(&self, proof: &[u8]) -> bool {
        proof == self.expected_proof.as_slice()
    }

    pub fn server_proof(&self) -> &[u8] {
        &self.server_proof
    }

    pub fn session_key(&self) -> &[u8] {
        &self.session_key
    }
}

#[cfg(test)]
mod tests {
    use sha1::Sha1;

    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn sha1(parts: &[&[u8]]) -> Vec<u8> {
        let mut hasher = Sha1::new();
        for part in parts {
            sha1::Digest::update(&mut hasher, part);
        }
        sha1::Digest::finalize(hasher).to_vec()
    }

    const N_1024: &str = "EEAF0AB9ADB38DD69C33F80AFA8FC5E86072618775FF3C0B9EA2314C9C256576D674DF7496EA81D3383B4813D692C6E0E0D5D8E250B98BE48E495C1D6089DAD15DC7D7B46154D6B6CE8EF4AD69B15D4982559B297BCF1885C529F566660E57EC68EDBC3C05726CC02FD4CBF4976EAA9AFD5138FE8376435B9FC61D2FC0EB06E3";
    const A_PUB: &str = "61d5e490f6f1b79547b0704c436f523dd0e560f0c64115bb72557ec44352e8903211c04692272d8b2d1a5358a2cf1b6e0bfcf99f921530ec8e39356179eae45e42ba92aeaced825171e1e8b9af6d9c03e1327f44be087ef06530e69f66615261eef54073ca11cf5858f0edfdfe15efeab349ef5d76988a3672fac47b0769447b";
    const B_PUB: &str = "bd0c61512c692c0cb6d041fa01bb152d4916a1e77af46ae105393011baf38964dc46a0670dd125b95a981652236f99d9b681cbf87837ec996c6da04453728610d0c6ddb58b318885d7d82c7f8deb75ce7bd4fbaa37089e6f9c6059f388838e7a00030b331eb76840910440b1b27aaeaeeb4012b7d7665238a8e3fb004b117b58";
    const SECRET: &str = "b0dc82babcf30674ae450c0287745e7990a3381f63b387aaf271a10d233861e359b48220f7c4693c9ae12b0a6f67809f0876e2d013800d6c41bb59b6d5979b5c00a172b4a2a5903a0bdcaf8a709585eb2afafa8f3499b200210dcc1f10eb33943cd67fc88a2f39a4be5bec4ec0a3212dc346d7e474b29ede8a469ffeca686e5a";

    // RFC 5054 appendix B (1024-bit group, g = 2, SHA-1) exercises the generic code path.
    #[test]
    fn rfc5054_vector() {
        let group = Group {
            n: BigUint::parse_bytes(N_1024.as_bytes(), 16).unwrap(),
            g: BigUint::from(2u8),
            width: 128,
        };
        let salt = unhex("BEB25379D1A8581EB5A727673A2441EE");
        let a = unhex("60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393");
        let b = unhex("E487CB59D31AC550471E81F00F6928E01DDA08E974A004F49E61F5D105284D20");
        let client = SrpClient::new(
            &group,
            sha1,
            b"alice",
            b"password123",
            &salt,
            &a,
            &unhex(B_PUB),
        )
        .unwrap();
        assert_eq!(client.public_a(&group), unhex(A_PUB));
        assert_eq!(client.session_key(), sha1(&[&unhex(SECRET)]).as_slice());
        let server = SrpServer::new(
            &group,
            sha1,
            b"alice",
            b"password123",
            &salt,
            &b,
            &unhex(A_PUB),
        )
        .unwrap();
        assert_eq!(server.public_b(&group), unhex(B_PUB));
        assert_eq!(server.session_key(), client.session_key());
        assert!(server.verify_client_proof(client.proof()));
        assert!(client.verify_server_proof(server.server_proof()));
    }

    #[test]
    fn hap_group_round_trip_and_wrong_password() {
        let group = Group::rfc5054_3072();
        let salt = [7u8; 16];
        let a = [3u8; 32];
        let b = [9u8; 32];
        let k_probe = SrpServer::new(
            &group,
            sha512,
            b"Pair-Setup",
            b"3939",
            &salt,
            &b,
            &[5u8; 384],
        )
        .unwrap();
        let server_pub = k_probe.public_b(&group);
        let client = SrpClient::new(
            &group,
            sha512,
            b"Pair-Setup",
            b"3939",
            &salt,
            &a,
            &server_pub,
        )
        .unwrap();
        let server = SrpServer::new(
            &group,
            sha512,
            b"Pair-Setup",
            b"3939",
            &salt,
            &b,
            &client.public_a(&group),
        )
        .unwrap();
        assert_eq!(server.public_b(&group), server_pub);
        assert_eq!(client.session_key().len(), 64);
        assert_eq!(client.session_key(), server.session_key());
        assert!(server.verify_client_proof(client.proof()));
        assert!(client.verify_server_proof(server.server_proof()));

        let wrong = SrpClient::new(
            &group,
            sha512,
            b"Pair-Setup",
            b"0000",
            &salt,
            &a,
            &server_pub,
        )
        .unwrap();
        assert!(!server.verify_client_proof(wrong.proof()));
    }

    #[test]
    fn rejects_degenerate_server_key() {
        let group = Group::rfc5054_3072();
        assert!(
            SrpClient::new(
                &group,
                sha512,
                b"Pair-Setup",
                b"3939",
                &[0; 16],
                &[1; 32],
                &[0; 384]
            )
            .is_err()
        );
    }
}
