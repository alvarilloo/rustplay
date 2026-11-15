use rustplay_core::crypto::fairplay::FairPlayClient;
use rustplay_core::crypto::hap::{PairClient, SessionKeys, Step};
use rustplay_core::crypto::stream::{audio_key, control_keys, control_keys_from};
use rustplay_core::identity::{Identity, PairingRecord};
use rustplay_core::{Error, Result};

use crate::conn::{RtspConn, ensure_ok};
use crate::info::{PairingKind, ReceiverInfo};

const OCTETS: &str = "application/octet-stream";

pub const HKP_LEGACY: u8 = 3;
pub const HKP_TRANSIENT: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Stored,
    Pin,
    Transient,
    TransientEmptyPassword,
    LegacyRaw,
    FairPlayOnly,
}

// Order of attempts: stored credentials, then the receiver's own demand (PIN), then transient.
pub fn plan(info: &ReceiverInfo, has_stored: bool) -> Vec<Method> {
    let mut methods = Vec::new();
    if has_stored {
        methods.push(Method::Stored);
    }
    match info.credential() {
        crate::info::Credential::Pin => methods.push(Method::Pin),
        crate::info::Credential::Password => {}
        crate::info::Credential::None => {
            if info.supports_transient() || info.uses_modern_pairing() {
                methods.push(Method::Transient);
                methods.push(Method::TransientEmptyPassword);
            } else {
                methods.push(Method::LegacyRaw);
                methods.push(Method::FairPlayOnly);
            }
        }
    }
    methods
}

pub fn pair_headers(info: &ReceiverInfo, hkp: u8, client_name: &str, pairing_id: &str, verify: bool) -> Vec<(String, String)> {
    let mut headers = vec![("X-Apple-HKP".to_owned(), hkp.to_string())];
    if info.pairing_kind() == PairingKind::Hap {
        headers.push(("X-Apple-Client-Name".to_owned(), client_name.to_owned()));
        headers.push(("X-Apple-Client-ID".to_owned(), pairing_id.to_owned()));
        if verify {
            headers.push(("X-Apple-PD".to_owned(), "1".to_owned()));
        }
    }
    headers
}

fn borrowed(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

async fn drive(
    conn: &mut RtspConn,
    what: &str,
    uri: &str,
    headers: &[(String, String)],
    mut step: impl FnMut(Option<&[u8]>) -> Result<Step>,
) -> Result<SessionKeys> {
    let mut reply: Option<Vec<u8>> = None;
    for _ in 0..8 {
        match step(reply.as_deref())? {
            Step::Done(keys) => return Ok(keys),
            Step::Reply(body) => {
                let response = conn
                    .call(what, "POST", uri, &borrowed(headers), Some(OCTETS), &body)
                    .await?;
                reply = Some(response.body.to_vec());
            }
        }
    }
    Err(Error::Pairing(format!("{what}: el intercambio no termina")))
}

pub struct PairingContext<'a> {
    pub identity: &'a Identity,
    pub info: &'a ReceiverInfo,
    pub client_name: &'a str,
}

impl PairingContext<'_> {
    fn headers(&self, hkp: u8, verify: bool) -> Vec<(String, String)> {
        pair_headers(self.info, hkp, self.client_name, &self.identity.pairing_id, verify)
    }

    fn transient_hkp(&self) -> u8 {
        match self.info.pairing_kind() {
            PairingKind::Hap => HKP_TRANSIENT,
            PairingKind::Legacy => HKP_LEGACY,
        }
    }
}

// pair-verify; on success the connection switches to the encrypted HAP framing.
async fn verify(
    conn: &mut RtspConn,
    ctx: &PairingContext<'_>,
    client: &mut PairClient<'_>,
    hkp: u8,
    peer_key: Option<&[u8; 32]>,
) -> Result<[u8; 32]> {
    let headers = ctx.headers(hkp, true);
    let keys = drive(conn, "pair-verify", "/pair-verify", &headers, |reply| {
        client.pair_verify(peer_key, reply)
    })
    .await?;
    let (write_key, read_key) = control_keys(&keys.shared_secret)?;
    conn.enable_encryption(read_key, write_key);
    Ok(keys.shared_secret)
}

pub async fn pair_stored(conn: &mut RtspConn, ctx: &PairingContext<'_>, record: &PairingRecord) -> Result<[u8; 32]> {
    let mut client = PairClient::new(ctx.identity);
    verify(conn, ctx, &mut client, HKP_LEGACY, Some(&record.peer_public_key)).await
}

// Transient pairing ends after M4: the control keys come from the SRP key and no pair-verify follows.
pub async fn pair_transient(conn: &mut RtspConn, ctx: &PairingContext<'_>, empty_password: bool) -> Result<Vec<u8>> {
    let mut client = PairClient::new(ctx.identity);
    if empty_password {
        client.transient_password = String::new();
    }
    let headers = ctx.headers(ctx.transient_hkp(), false);
    let keys = drive(conn, "pair-setup", "/pair-setup", &headers, |reply| client.pair_setup(None, reply)).await?;
    let (write_key, read_key) = control_keys_from(&keys.secret)?;
    conn.enable_encryption(read_key, write_key);
    Ok(keys.secret)
}

// Original (non-HAP) pairing: raw Ed25519 key exchange, the connection stays in plaintext.
// Returns the verify secret when the receiver mixes it into the FairPlay key.
pub async fn pair_legacy_raw(conn: &mut RtspConn, ctx: &PairingContext<'_>) -> Result<Option<[u8; 32]>> {
    let mut client = PairClient::new(ctx.identity);
    let keys = drive(conn, "pair-setup", "/pair-setup", &[], |reply| client.pair_setup_legacy(reply)).await?;
    let server_key = keys
        .peer
        .map(|r| r.peer_public_key)
        .or(ctx.info.public_key)
        .ok_or_else(|| Error::Pairing("el receptor no envió su clave pública".into()))?;
    let mix = legacy_mixes_secret(ctx.info);
    let headers: Vec<(String, String)> = if mix {
        vec![("X-Apple-PD".to_owned(), "1".to_owned())]
    } else {
        Vec::new()
    };
    let keys = drive(conn, "pair-verify", "/pair-verify", &headers, |reply| {
        client.pair_verify_legacy(&server_key, reply)
    })
    .await?;
    Ok(mix.then_some(keys.shared_secret))
}

// Receivers advertising the original pairing (feature 27) use the unmixed FairPlay key.
pub fn legacy_mixes_secret(info: &ReceiverInfo) -> bool {
    !info.supports_legacy_pairing()
}

pub struct PinPairing {
    pub shared_secret: [u8; 32],
    pub record: Option<PairingRecord>,
}

pub async fn start_pin_display(conn: &mut RtspConn, ctx: &PairingContext<'_>) -> Result<()> {
    let headers = ctx.headers(HKP_LEGACY, false);
    let response = conn
        .request("POST", "/pair-pin-start", &borrowed(&headers), None, &[])
        .await?;
    // 453: the receiver accepted the request and shows the PIN asynchronously.
    if response.status == 453 {
        return Ok(());
    }
    ensure_ok("pair-pin-start", &response)
}

pub async fn pair_pin(conn: &mut RtspConn, ctx: &PairingContext<'_>, pin: &str) -> Result<PinPairing> {
    let mut client = PairClient::new(ctx.identity);
    let headers = ctx.headers(HKP_LEGACY, false);
    let keys = drive(conn, "pair-setup", "/pair-setup", &headers, |reply| {
        client.pair_setup(Some(pin), reply)
    })
    .await?;
    let record = keys.peer;
    let peer_key = record.as_ref().map(|r| r.peer_public_key);
    let shared_secret = verify(conn, ctx, &mut client, HKP_LEGACY, peer_key.as_ref()).await?;
    Ok(PinPairing { shared_secret, record })
}

pub struct FairPlayKeys {
    pub ekey: [u8; 72],
    pub eiv: [u8; 16],
    pub audio_key: [u8; 16],
}

// The raw key is mixed with the pair-verify secret only when pairing produced one.
pub fn mix_audio_key(raw: &[u8; 16], shared_secret: Option<&[u8; 32]>) -> [u8; 16] {
    match shared_secret {
        Some(secret) => audio_key(raw, secret),
        None => *raw,
    }
}

pub async fn fairplay_setup(conn: &mut RtspConn, shared_secret: Option<&[u8; 32]>) -> Result<FairPlayKeys> {
    let mut fairplay = FairPlayClient::new();
    let headers = [("X-Apple-ET", "32")];
    let m1 = fairplay.message1();
    let m2 = conn
        .call("fp-setup (1)", "POST", "/fp-setup", &headers, Some(OCTETS), &m1)
        .await?;
    let m3 = fairplay.exchange(&m2.body)?;
    let m4 = conn
        .call("fp-setup (2)", "POST", "/fp-setup", &headers, Some(OCTETS), &m3)
        .await?;
    fairplay.confirm(&m4.body)?;
    let mut raw = [0u8; 16];
    rand::fill(&mut raw[..]);
    let mut eiv = [0u8; 16];
    rand::fill(&mut eiv[..]);
    let ekey = fairplay.wrap_key(&raw)?;
    Ok(FairPlayKeys {
        ekey,
        eiv,
        audio_key: mix_audio_key(&raw, shared_secret),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::fixtures::*;

    #[test]
    fn plan_prefers_stored_then_receiver_demand() {
        assert_eq!(plan(&apple_tv(), true)[..2], [Method::Stored, Method::Transient]);
        assert_eq!(plan(&apple_tv(), false), [Method::Transient, Method::TransientEmptyPassword]);
        assert_eq!(plan(&pin_receiver(), false), [Method::Pin]);
        assert_eq!(plan(&pin_receiver(), true), [Method::Stored, Method::Pin]);
    }

    #[test]
    fn legacy_receivers_without_transient_use_raw_pairing_then_fairplay() {
        assert_eq!(plan(&legacy_receiver(), false), [Method::LegacyRaw, Method::FairPlayOnly]);
        assert!(!legacy_mixes_secret(&legacy_receiver()));
    }

    #[test]
    fn headers_depend_on_pairing_family() {
        let hap = pair_headers(&apple_tv(), HKP_TRANSIENT, "PC", "ID-1", true);
        assert!(hap.contains(&("X-Apple-HKP".into(), "4".into())));
        assert!(hap.contains(&("X-Apple-Client-ID".into(), "ID-1".into())));
        assert!(hap.contains(&("X-Apple-PD".into(), "1".into())));
        let legacy = pair_headers(&legacy_receiver(), HKP_LEGACY, "PC", "ID-1", true);
        assert_eq!(legacy, [("X-Apple-HKP".to_owned(), "3".to_owned())]);
    }

    #[test]
    fn audio_key_is_mixed_only_with_a_shared_secret() {
        let raw = [3u8; 16];
        assert_eq!(mix_audio_key(&raw, None), raw);
        let mixed = mix_audio_key(&raw, Some(&[4u8; 32]));
        assert_ne!(mixed, raw);
        assert_eq!(mixed, audio_key(&raw, &[4u8; 32]));
    }
}
