//! Dual-face (multi-homed edge proxy) plane classification — RFC 3261 §16.
//!
//! The kind deployment splits into two network planes: an **internal** plane
//! (proxy ↔ b2bua workers; pod/node CIDRs; the existing VRRP VIP) and an
//! **external** plane (a dedicated no-NAT bridge where every caller lives,
//! with its own VRRP VIP). The proxy is the ONLY component bridging the two —
//! it binds one UDP socket per face and picks the egress socket (and the
//! advertise stamped on Via / Record-Route) **by destination IP**: an address
//! inside any configured internal CIDR egresses on the internal face,
//! everything else on the external face.
//!
//! [`FaceCidrs`] is that classifier: the parsed `PROXY_FACE_INT_CIDRS`
//! comma-separated IPv4 CIDR list. It is deliberately IPv4-only (the cluster
//! planes are IPv4; an IPv6 destination classifies external) and fail-fast:
//! an empty or unparseable list is a boot refusal, never a silent
//! default-to-external (which would leak worker traffic onto the caller
//! plane).

use std::net::{IpAddr, Ipv4Addr};

/// One `a.b.c.d/len` IPv4 CIDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Cidr {
    net: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Cidr {
    /// Parse `"10.244.0.0/16"`. Rejects a missing/out-of-range prefix or a
    /// non-IPv4 network. Host bits below the prefix are masked off (so
    /// `10.244.1.2/16` classifies like `10.244.0.0/16`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (net, prefix) = s
            .split_once('/')
            .ok_or_else(|| format!("CIDR {s:?} missing '/prefix' (want e.g. 10.244.0.0/16)"))?;
        let net: Ipv4Addr = net
            .trim()
            .parse()
            .map_err(|e| format!("CIDR {s:?} has an unparseable IPv4 network: {e}"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .map_err(|e| format!("CIDR {s:?} has an unparseable prefix: {e}"))?;
        if prefix > 32 {
            return Err(format!("CIDR {s:?} prefix {prefix} out of range 0..=32"));
        }
        Ok(Self { net: Ipv4Addr::from(u32::from(net) & Self::mask(prefix)), prefix })
    }

    fn mask(prefix: u8) -> u32 {
        if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        }
    }

    /// Is `ip` inside this CIDR?
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & Self::mask(self.prefix)) == u32::from(self.net)
    }
}

/// The internal-plane classifier: the parsed `PROXY_FACE_INT_CIDRS` list.
/// Destination IP ∈ any listed CIDR → internal face; else → external face.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceCidrs {
    cidrs: Vec<Ipv4Cidr>,
}

impl FaceCidrs {
    /// Parse a comma-separated CIDR list (`"10.244.0.0/16,172.20.0.0/16"`).
    /// An empty list or any unparseable entry is an `Err` — dual-face mode
    /// must fail fast at boot rather than mis-plane traffic.
    pub fn parse(list: &str) -> Result<Self, String> {
        let cidrs: Vec<Ipv4Cidr> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Ipv4Cidr::parse)
            .collect::<Result<_, _>>()?;
        if cidrs.is_empty() {
            return Err(
                "internal-face CIDR list is empty — dual-face mode needs at least one \
                 internal CIDR (e.g. PROXY_FACE_INT_CIDRS=10.244.0.0/16,172.20.0.0/16)"
                    .to_string(),
            );
        }
        Ok(Self { cidrs })
    }

    /// Is `ip` on the internal plane? IPv6 never matches (the cluster planes
    /// are IPv4) — an IPv6 destination classifies external.
    pub fn contains_ip(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => self.cidrs.iter().any(|c| c.contains(v4)),
            IpAddr::V6(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_classifies() {
        let f = FaceCidrs::parse("10.244.0.0/16, 172.20.0.0/16").unwrap();
        assert!(f.contains_ip("10.244.3.7".parse().unwrap()));
        assert!(f.contains_ip("172.20.255.250".parse().unwrap()));
        assert!(!f.contains_ip("192.168.60.10".parse().unwrap()));
        assert!(!f.contains_ip("10.245.0.1".parse().unwrap()));
    }

    #[test]
    fn masks_host_bits() {
        let c = Ipv4Cidr::parse("10.244.9.9/16").unwrap();
        assert!(c.contains("10.244.0.1".parse().unwrap()));
        assert!(!c.contains("10.243.0.1".parse().unwrap()));
    }

    #[test]
    fn prefix_edges() {
        let all = Ipv4Cidr::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
        let one = Ipv4Cidr::parse("10.0.0.1/32").unwrap();
        assert!(one.contains("10.0.0.1".parse().unwrap()));
        assert!(!one.contains("10.0.0.2".parse().unwrap()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Ipv4Cidr::parse("10.0.0.0").is_err(), "missing prefix");
        assert!(Ipv4Cidr::parse("10.0.0.0/33").is_err(), "prefix out of range");
        assert!(Ipv4Cidr::parse("nope/8").is_err(), "bad network");
        assert!(Ipv4Cidr::parse("::1/64").is_err(), "IPv6 network rejected");
        assert!(FaceCidrs::parse("").is_err(), "empty list rejected");
        assert!(FaceCidrs::parse("10.0.0.0/8,bogus").is_err(), "one bad entry poisons the list");
    }

    #[test]
    fn ipv6_destination_is_external() {
        let f = FaceCidrs::parse("10.0.0.0/8").unwrap();
        assert!(!f.contains_ip("::1".parse().unwrap()));
    }
}
