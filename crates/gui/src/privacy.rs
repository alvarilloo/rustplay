// Optional hiding of IP addresses in the interface. The log always keeps them.
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

static HIDE_IPS: AtomicBool = AtomicBool::new(false);

const IPV4_MASK: &str = "•••.•••.•••.•••";
const IPV6_MASK: &str = "••••:••••";

pub fn set_hide_ips(hide: bool) {
    HIDE_IPS.store(hide, Ordering::Release);
}

pub fn hide_ips() -> bool {
    HIDE_IPS.load(Ordering::Acquire)
}

// Masks the text only when the option is on.
pub fn redact(text: &str) -> Cow<'_, str> {
    if hide_ips() { mask_ips(text) } else { Cow::Borrowed(text) }
}

// Replaces IPv4 addresses (a.b.c.d) and IPv6 addresses (groups of hex digits joined by at least
// two colons, e.g. fe80::1c2:3) with a fixed mask. Ports after an IPv4 address are kept.
pub fn mask_ips(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !is_token_byte(bytes[i - 1]);
        if boundary && let Some((end, mask)) = match_ipv4(bytes, i).or_else(|| match_ipv6(bytes, i)) {
            out.push_str(&text[copied..i]);
            out.push_str(mask);
            copied = end;
            i = end;
            continue;
        }
        i += 1;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b':'
}

fn match_ipv4(b: &[u8], start: usize) -> Option<(usize, &'static str)> {
    let mut i = start;
    for part in 0..4 {
        let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || digits > 3 {
            return None;
        }
        let value: u32 = std::str::from_utf8(&b[i..i + digits]).ok()?.parse().ok()?;
        if value > 255 {
            return None;
        }
        i += digits;
        if part < 3 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    // "1.2.3.4.5" or "1.2.3.4abc" are not addresses.
    if b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || (*c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) {
        return None;
    }
    Some((i, IPV4_MASK))
}

fn match_ipv6(b: &[u8], start: usize) -> Option<(usize, &'static str)> {
    let len = b[start..].iter().take_while(|c| c.is_ascii_hexdigit() || **c == b':').count();
    let token = &b[start..start + len];
    let colons = token.iter().filter(|c| **c == b':').count();
    let groups_ok = token.split(|c| *c == b':').all(|g| g.len() <= 4);
    let has_hex = token.iter().any(u8::is_ascii_hexdigit);
    // Times such as 16:12:59 have two colons too, so require a double colon or 6+ colons.
    let looks_ipv6 = colons >= 2 && groups_ok && has_hex && (token.windows(2).any(|w| w == b"::") || colons >= 6);
    if !looks_ipv6 || b.get(start + len).is_some_and(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((start + len, IPV6_MASK))
}

#[cfg(test)]
mod tests {
    use super::mask_ips;

    #[test]
    fn ipv4_addresses_are_masked_and_ports_kept() {
        assert_eq!(mask_ips("conexión de 192.168.1.11:54096 ok"), "conexión de •••.•••.•••.•••:54096 ok");
        assert_eq!(mask_ips("[\"55\\\" QLED\"] 192.168.1.10"), "[\"55\\\" QLED\"] •••.•••.•••.•••");
    }

    #[test]
    fn versions_times_and_other_numbers_are_left_alone() {
        for text in ["RustPlay 0.1.0 iniciado", "16:12:59.823 INFO", "AirPlay/1005.13.1", "1.2.3.4.5", "rtp=1616747859", "300.1.1.1"] {
            assert_eq!(mask_ips(text), text, "{text}");
        }
    }

    #[test]
    fn ipv6_addresses_are_masked() {
        assert_eq!(mask_ips("desde fe80::1c2:3%12"), "desde ••••:••••%12");
        assert_eq!(mask_ips("[2001:db8:0:0:0:0:2:1]:7000"), "[••••:••••]:7000");
    }
}
