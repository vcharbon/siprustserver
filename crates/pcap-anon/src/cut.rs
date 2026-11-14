//! The SUT address set: the per-source-network IP/subnet knowledge of which
//! sockets ARE the system under test. A network profile ([`crate::profile`])
//! carries one; the anonymiser keeps the addresses it names
//! ([`crate::fake::address::Scope`]). Format (JSON):
//!
//! ```json
//! { "sut": ["10.20.30.68", "198.51.100.0/24"] }
//! ```
//!
//! - `sut` — literal IPv4 addresses or `a.b.c.d/prefix` CIDR blocks that are
//!   the SUT's signalling sockets. Matching is address-only (port ignored):
//!   the cut is a network-topology fact, not a socket-level one.

use serde::Deserialize;

/// The loaded cut config (see module doc for the on-disk format).
#[derive(Debug, Clone, Deserialize)]
pub struct Cut {
    pub sut: Vec<String>,
}

impl Cut {
    /// Parse + validate a cut document; every violation is a loud error.
    pub fn from_json_str(json: &str) -> Result<Cut, String> {
        let cut: Cut =
            serde_json::from_str(json).map_err(|e| format!("cut config is not valid: {e}"))?;
        for entry in &cut.sut {
            parse_matcher(entry).ok_or_else(|| {
                format!("cut: {entry:?} is not an IPv4 address or a.b.c.d/prefix")
            })?;
        }
        Ok(cut)
    }

    /// Whether an `"ip:port"` socket's address, or a bare address, is in the
    /// SUT set.
    pub fn matches(&self, sock: &str) -> bool {
        let Some(ip) = sock_ip(sock) else { return false };
        self.sut.iter().any(|entry| match parse_matcher(entry) {
            Some(Matcher::Exact(a)) => a == ip,
            Some(Matcher::Cidr { base, bits }) => in_cidr(ip, base, bits),
            None => false,
        })
    }
}

/// A parsed SUT matcher.
enum Matcher {
    Exact(u32),
    Cidr { base: u32, bits: u8 },
}

fn parse_matcher(entry: &str) -> Option<Matcher> {
    match entry.split_once('/') {
        Some((addr, prefix)) => {
            let base = parse_ipv4(addr)?;
            let bits: u8 = prefix.parse().ok()?;
            (bits <= 32).then_some(Matcher::Cidr { base, bits })
        }
        None => Some(Matcher::Exact(parse_ipv4(entry)?)),
    }
}

/// The address of an `"ip:port"` string (IPv6 unsupported — the corpus is v4).
fn sock_ip(sock: &str) -> Option<u32> {
    parse_ipv4(sock.rsplit_once(':').map(|(a, _)| a).unwrap_or(sock))
}

fn parse_ipv4(s: &str) -> Option<u32> {
    let mut octets = [0u32; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 {
            return None;
        }
        let v: u32 = part.parse().ok()?;
        if v > 255 {
            return None;
        }
        octets[n] = v;
        n += 1;
    }
    (n == 4).then(|| (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3])
}

fn in_cidr(ip: u32, base: u32, bits: u8) -> bool {
    if bits == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - bits as u32);
    (ip & mask) == (base & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_address_and_a_cidr_block_match_their_sockets_and_nothing_else() {
        let cut = Cut::from_json_str(r#"{"sut":["10.20.30.68","198.51.100.0/24"]}"#).unwrap();
        assert!(cut.matches("10.20.30.68:5060"));
        assert!(cut.matches("10.20.30.68"));
        assert!(!cut.matches("10.20.30.146:5071"));
        assert!(cut.matches("198.51.100.9:5060"));
        assert!(!cut.matches("198.51.101.9:5060"));
    }

    #[test]
    fn invalid_cut_is_rejected_loudly() {
        assert!(Cut::from_json_str(r#"{"sut":["not-an-ip"]}"#).is_err());
        assert!(Cut::from_json_str(r#"{"sut":["10.0.0.0/33"]}"#).is_err());
    }
}
