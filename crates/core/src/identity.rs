use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey};
use plist::{Dictionary, Value};
use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

use crate::{Error, Result};

const IDENTITY_FILE: &str = "identity.plist";
const PAIRINGS_FILE: &str = "pairings.plist";
// Senders (iPhone, Mac...) paired with our receiver through the PIN flow; kept apart from the
// accessories the sender side paired with.
const CLIENT_PAIRINGS_FILE: &str = "client-pairings.plist";

pub struct Identity {
    pub device_id: [u8; 6],
    pub signing_key: SigningKey,
    pub pairing_id: String,
}

impl Identity {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_in(&data_dir()?)
    }

    pub fn load_or_create_in(dir: &Path) -> Result<Self> {
        let path = dir.join(IDENTITY_FILE);
        if path.exists() {
            match Self::read(&path) {
                Ok(identity) => return Ok(identity),
                Err(e) => tracing::warn!("identidad ilegible ({e}); se generará una nueva"),
            }
        }
        let identity = Self::generate();
        identity.write(&path)?;
        Ok(identity)
    }

    fn generate() -> Self {
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]);
        Self {
            device_id: primary_mac().unwrap_or_else(random_mac),
            signing_key: SigningKey::from_bytes(&secret),
            pairing_id: uuid::Uuid::new_v4().to_string().to_uppercase(),
        }
    }

    fn read(path: &Path) -> Result<Self> {
        let value = Value::from_file(path)?;
        let dict = value
            .as_dictionary()
            .ok_or_else(|| Error::protocol("identity.plist no es un diccionario"))?;
        let device_id = fixed_data::<6>(dict, "device_id")?;
        let secret = fixed_data::<32>(dict, "signing_key")?;
        let pairing_id = dict
            .get("pairing_id")
            .and_then(Value::as_string)
            .ok_or_else(|| Error::protocol("identity.plist sin pairing_id"))?
            .to_owned();
        Ok(Self {
            device_id,
            signing_key: SigningKey::from_bytes(&secret),
            pairing_id,
        })
    }

    fn write(&self, path: &Path) -> Result<()> {
        let mut dict = Dictionary::new();
        dict.insert("device_id".into(), Value::Data(self.device_id.to_vec()));
        dict.insert(
            "signing_key".into(),
            Value::Data(self.signing_key.to_bytes().to_vec()),
        );
        dict.insert("pairing_id".into(), Value::String(self.pairing_id.clone()));
        write_plist(path, Value::Dictionary(dict))
    }

    pub fn device_id_string(&self) -> String {
        let d = &self.device_id;
        format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            d[0], d[1], d[2], d[3], d[4], d[5]
        )
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
}

#[derive(Debug, Clone)]
pub struct PairingRecord {
    pub peer_id: String,
    pub peer_public_key: [u8; 32],
}

pub struct PairingStore {
    pub path: PathBuf,
    pub records: Vec<PairingRecord>,
}

impl PairingStore {
    pub fn load() -> Result<Self> {
        Self::load_from(data_dir()?.join(PAIRINGS_FILE))
    }

    pub fn load_from(path: PathBuf) -> Result<Self> {
        let mut records = Vec::new();
        if path.exists() {
            let value = Value::from_file(&path)?;
            let items = value
                .as_array()
                .ok_or_else(|| Error::protocol("pairings.plist no es una lista"))?;
            for item in items {
                let dict = item
                    .as_dictionary()
                    .ok_or_else(|| Error::protocol("registro de emparejamiento no válido"))?;
                let peer_id = dict
                    .get("peer_id")
                    .and_then(Value::as_string)
                    .ok_or_else(|| Error::protocol("registro sin peer_id"))?
                    .to_owned();
                records.push(PairingRecord {
                    peer_id,
                    peer_public_key: fixed_data::<32>(dict, "public_key")?,
                });
            }
        }
        Ok(Self { path, records })
    }

    pub fn load_clients() -> Result<Self> {
        Self::load_from(client_pairings_path()?)
    }

    pub fn find(&self, peer_id: &str) -> Option<&PairingRecord> {
        self.records.iter().find(|r| r.peer_id == peer_id)
    }

    pub fn find_key(&self, public_key: &[u8; 32]) -> Option<&PairingRecord> {
        self.records.iter().find(|r| &r.peer_public_key == public_key)
    }

    // Removes every record of `peer_id` (compared without case); returns whether one existed.
    pub fn remove(&mut self, peer_id: &str) -> Result<bool> {
        let before = self.records.len();
        self.records.retain(|r| !r.peer_id.eq_ignore_ascii_case(peer_id));
        if self.records.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    pub fn upsert(&mut self, record: PairingRecord) -> Result<()> {
        match self
            .records
            .iter_mut()
            .find(|r| r.peer_id == record.peer_id)
        {
            Some(existing) => *existing = record,
            None => self.records.push(record),
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let items = self
            .records
            .iter()
            .map(|r| {
                let mut dict = Dictionary::new();
                dict.insert("peer_id".into(), Value::String(r.peer_id.clone()));
                dict.insert("public_key".into(), Value::Data(r.peer_public_key.to_vec()));
                Value::Dictionary(dict)
            })
            .collect();
        write_plist(&self.path, Value::Array(items))
    }
}

// Portable mode: a file named `portable.txt` next to RustPlay.exe keeps every piece of data in a
// `data` folder next to the executable instead of %APPDATA%\RustPlay.
pub const PORTABLE_MARKER: &str = "portable.txt";
const PORTABLE_DATA: &str = "data";

// The portable data folder for an executable located in `exe_dir`, when the marker file is there.
pub fn portable_data_dir_in(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir.join(PORTABLE_MARKER).is_file().then(|| exe_dir.join(PORTABLE_DATA))
}

// Folder holding RustPlay.exe when it runs in portable mode.
pub fn portable_exe_dir() -> Option<PathBuf> {
    static DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        portable_data_dir_in(&dir).map(|_| dir)
    })
    .clone()
}

pub fn is_portable() -> bool {
    portable_exe_dir().is_some()
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(exe_dir) = portable_exe_dir() {
        let dir = exe_dir.join(PORTABLE_DATA);
        std::fs::create_dir_all(&dir)?;
        return Ok(dir);
    }
    let base = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::protocol("la variable APPDATA no está definida"))?;
    let dir = PathBuf::from(base).join(DATA_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn client_pairings_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(CLIENT_PAIRINGS_FILE))
}

const DATA_DIR: &str = "RustPlay";

fn fixed_data<const N: usize>(dict: &Dictionary, key: &str) -> Result<[u8; N]> {
    match dict.get(key) {
        Some(Value::Data(bytes)) => bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::protocol(format!("«{key}» tiene una longitud incorrecta"))),
        _ => Err(Error::protocol(format!("falta «{key}»"))),
    }
}

fn write_plist(path: &Path, value: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("plist.tmp");
    value.to_file_xml(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn random_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    rand::fill(&mut mac[..]);
    mac[0] = (mac[0] | 0x02) & 0xFE;
    mac
}

fn primary_mac() -> Option<[u8; 6]> {
    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;
    const VIRTUAL_MARKERS: [&str; 8] = [
        "virtual",
        "vmware",
        "vbox",
        "hyper-v",
        "loopback",
        "tap-",
        "vpn",
        "bluetooth",
    ];

    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        buf.resize((size as usize).div_ceil(8), 0);
        let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: the buffer is at least `size` bytes and 8-byte aligned.
        let rc = unsafe { GetAdaptersAddresses(0, flags, None, Some(head), &mut size) };
        if rc == ERROR_BUFFER_OVERFLOW.0 {
            continue;
        }
        if rc != 0 {
            return None;
        }
        let mut best: Option<(u8, [u8; 6])> = None;
        let mut cur = head.cast_const();
        // SAFETY: the list was filled by GetAdaptersAddresses and lives in `buf`.
        while let Some(adapter) = unsafe { cur.as_ref() } {
            cur = adapter.Next;
            if adapter.OperStatus != IfOperStatusUp
                || !(adapter.IfType == IF_TYPE_ETHERNET || adapter.IfType == IF_TYPE_WIFI)
                || adapter.PhysicalAddressLength != 6
            {
                continue;
            }
            let description = unsafe { adapter.Description.to_string() }
                .unwrap_or_default()
                .to_lowercase();
            let is_virtual = VIRTUAL_MARKERS.iter().any(|m| description.contains(m));
            let has_gateway = !adapter.FirstGatewayAddress.is_null();
            let score = u8::from(has_gateway) * 2 + u8::from(!is_virtual);
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&adapter.PhysicalAddress[..6]);
            if mac == [0u8; 6] {
                continue;
            }
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, mac));
            }
        }
        return best.map(|(_, mac)| mac);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustplay-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_is_persisted_and_reloaded() {
        let dir = temp_dir("identity");
        let first = Identity::load_or_create_in(&dir).unwrap();
        let second = Identity::load_or_create_in(&dir).unwrap();
        assert_eq!(first.device_id, second.device_id);
        assert_eq!(first.signing_key.to_bytes(), second.signing_key.to_bytes());
        assert_eq!(first.pairing_id, second.pairing_id);
        assert_eq!(first.pairing_id, first.pairing_id.to_uppercase());
        assert_eq!(first.pairing_id.len(), 36);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn corrupt_identity_is_regenerated() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(IDENTITY_FILE), b"basura").unwrap();
        let identity = Identity::load_or_create_in(&dir).unwrap();
        assert_eq!(identity.device_id_string().len(), 17);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn portable_marker_selects_data_folder_next_to_the_exe() {
        let dir = temp_dir("portable");
        assert_eq!(portable_data_dir_in(&dir), None);
        std::fs::write(dir.join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(portable_data_dir_in(&dir), Some(dir.join("data")));
        // A folder with that name is not the marker file.
        let other = temp_dir("portable-dir");
        std::fs::create_dir_all(other.join(PORTABLE_MARKER)).unwrap();
        assert_eq!(portable_data_dir_in(&other), None);
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(other).ok();
    }

    #[test]
    fn random_mac_is_local_unicast() {
        let mac = random_mac();
        assert_eq!(mac[0] & 0x03, 0x02);
    }

    #[test]
    fn pairing_store_roundtrip_and_upsert() {
        let dir = temp_dir("pairings");
        let path = dir.join(PAIRINGS_FILE);
        let mut store = PairingStore::load_from(path.clone()).unwrap();
        assert!(store.records.is_empty());
        store
            .upsert(PairingRecord {
                peer_id: "A".into(),
                peer_public_key: [1; 32],
            })
            .unwrap();
        store
            .upsert(PairingRecord {
                peer_id: "B".into(),
                peer_public_key: [2; 32],
            })
            .unwrap();
        store
            .upsert(PairingRecord {
                peer_id: "A".into(),
                peer_public_key: [3; 32],
            })
            .unwrap();
        let reloaded = PairingStore::load_from(path).unwrap();
        assert_eq!(reloaded.records.len(), 2);
        assert_eq!(reloaded.find("A").unwrap().peer_public_key, [3; 32]);
        assert_eq!(reloaded.find("B").unwrap().peer_public_key, [2; 32]);
        assert!(reloaded.find("C").is_none());
        assert_eq!(reloaded.find_key(&[2; 32]).unwrap().peer_id, "B");
        let mut reloaded = reloaded;
        assert!(reloaded.remove("a").unwrap());
        assert!(!reloaded.remove("A").unwrap());
        let again = PairingStore::load_from(reloaded.path.clone()).unwrap();
        assert_eq!(again.records.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
