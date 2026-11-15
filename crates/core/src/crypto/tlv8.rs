use crate::{Error, Result};

pub const METHOD: u8 = 0x00;
pub const IDENTIFIER: u8 = 0x01;
pub const SALT: u8 = 0x02;
pub const PUBLIC_KEY: u8 = 0x03;
pub const PROOF: u8 = 0x04;
pub const ENCRYPTED_DATA: u8 = 0x05;
pub const STATE: u8 = 0x06;
pub const ERROR: u8 = 0x07;
pub const RETRY_DELAY: u8 = 0x08;
pub const CERTIFICATE: u8 = 0x09;
pub const SIGNATURE: u8 = 0x0a;
pub const PERMISSIONS: u8 = 0x0b;
pub const FLAGS: u8 = 0x13;
pub const SEPARATOR: u8 = 0xff;

// Values longer than 255 bytes are split into consecutive fragments of the same type.
// Two adjacent items of the same type must be separated by a `SEPARATOR` item.
pub fn encode(items: &[(u8, &[u8])], out: &mut Vec<u8>) {
    for &(tag, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[tag, 0]);
            continue;
        }
        for chunk in value.chunks(255) {
            out.push(tag);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
}

pub fn decode(buf: &[u8]) -> Result<Vec<(u8, Vec<u8>)>> {
    let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut pos = 0;
    let mut last_full = false;
    while pos < buf.len() {
        if pos + 2 > buf.len() {
            return Err(Error::protocol("TLV8 truncado"));
        }
        let tag = buf[pos];
        let len = usize::from(buf[pos + 1]);
        pos += 2;
        let value = buf
            .get(pos..pos + len)
            .ok_or_else(|| Error::protocol("TLV8 truncado"))?;
        pos += len;
        match items.last_mut() {
            Some((last_tag, data)) if last_full && *last_tag == tag => {
                data.extend_from_slice(value)
            }
            _ => items.push((tag, value.to_vec())),
        }
        last_full = len == 255;
    }
    Ok(items)
}

pub fn find(items: &[(u8, Vec<u8>)], tag: u8) -> Option<&[u8]> {
    items
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, v)| v.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_simple_items() {
        let mut out = Vec::new();
        encode(
            &[(STATE, &[1]), (METHOD, &[]), (IDENTIFIER, b"ab")],
            &mut out,
        );
        assert_eq!(out, [0x06, 1, 1, 0x00, 0, 0x01, 2, b'a', b'b']);
    }

    #[test]
    fn roundtrip_fragments_long_values() {
        let long: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        encode(&[(PUBLIC_KEY, &long), (STATE, &[2])], &mut out);
        assert_eq!(out[0], PUBLIC_KEY);
        assert_eq!(out[1], 255);
        assert_eq!(out[257], PUBLIC_KEY);
        assert_eq!(out.len(), 700 + 3 * 2 + 3);
        let items = decode(&out).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(find(&items, PUBLIC_KEY).unwrap(), long.as_slice());
        assert_eq!(find(&items, STATE).unwrap(), [2]);
    }

    #[test]
    fn exact_255_is_not_merged_with_next_item_of_other_type() {
        let value = [7u8; 255];
        let mut out = Vec::new();
        encode(&[(SALT, &value), (STATE, &[3])], &mut out);
        let items = decode(&out).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1.len(), 255);
    }

    #[test]
    fn separator_keeps_same_type_items_apart() {
        let mut out = Vec::new();
        encode(
            &[(IDENTIFIER, &[1]), (SEPARATOR, &[]), (IDENTIFIER, &[2])],
            &mut out,
        );
        let items = decode(&out).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2], (IDENTIFIER, vec![2]));
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(decode(&[0x06]).is_err());
        assert!(decode(&[0x06, 3, 1]).is_err());
        assert!(decode(&[]).unwrap().is_empty());
    }
}
