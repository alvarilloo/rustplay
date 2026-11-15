use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// Address to listen on for traffic from `peer`: every interface, or only the loopback one when the
// peer is this same machine (so local tests never trigger a Windows Firewall prompt).
pub fn listen_addr(peer: IpAddr) -> IpAddr {
    match peer {
        IpAddr::V4(ip) if ip.is_loopback() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_loopback() => Ipv6Addr::LOCALHOST.into(),
        IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
        IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_peers_get_loopback_listeners() {
        assert_eq!(listen_addr(Ipv4Addr::LOCALHOST.into()), IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_eq!(listen_addr(Ipv6Addr::LOCALHOST.into()), IpAddr::from(Ipv6Addr::LOCALHOST));
        assert_eq!(listen_addr(Ipv4Addr::new(192, 168, 1, 11).into()), IpAddr::from(Ipv4Addr::UNSPECIFIED));
        assert_eq!(listen_addr("fe80::1".parse().unwrap()), IpAddr::from(Ipv6Addr::UNSPECIFIED));
    }
}
