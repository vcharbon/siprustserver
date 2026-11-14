//! The fake of an IP address; the [`Scope`] of an anonymisation says which
//! addresses it maps. A fake keeps the text width of the address it replaces,
//! so a body keeps its Content-Length, and is a pure function of (seed,
//! address text). It lies in a range no real peer uses — `0.0.0.0/8` or
//! `240.0.0.0/4` for IPv4, `fd00::/8` for IPv6 — and no scope maps those
//! ranges, so a second pass leaves every fake as it is. An IPv4-mapped IPv6
//! address keeps its prefix and takes the fake of its IPv4 address.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::fake::{digest, Seed};

pub use crate::fake::scope::Scope;

/// The ranges no fake is ever drawn from and no real peer is ever reached at:
/// "this network", loopback, documentation, benchmarking, multicast and the
/// reserved `240.0.0.0/4`, where the fakes themselves live with `0.0.0.0/8`.
pub fn is_reserved_v4(addr: Ipv4Addr) -> bool {
    let [a, b, c, _] = addr.octets();
    a == 0
        || a == 127
        || a >= 224
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
}

/// Whether an address is reachable from the public internet: none of the
/// private, shared, link-local or protocol-assignment ranges.
pub(crate) fn is_public_v4(addr: Ipv4Addr) -> bool {
    let [a, b, c, _] = addr.octets();
    !(is_reserved_v4(addr)
        || a == 10
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0))
}

/// The dotted-quad octets of `text` with their text widths, when it is one
/// (each octet one to three digits and at most 255).
pub fn ipv4_octets(text: &str) -> Option<([u8; 4], [usize; 4])> {
    let mut octets = [0u8; 4];
    let mut widths = [0usize; 4];
    let mut parts = text.split('.');
    for i in 0..4 {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        octets[i] = u8::try_from(part.parse::<u16>().ok()?).ok()?;
        widths[i] = part.len();
    }
    parts.next().is_none().then_some((octets, widths))
}

/// How many octet values a text width holds: `0`–`9`, `10`–`99`, `100`–`255`.
fn values_of(width: usize) -> u128 {
    match width {
        1 => 10,
        2 => 90,
        _ => 156,
    }
}

/// The octet value `index` of a width (`index` below [`values_of`]).
fn octet(width: usize, index: u128) -> u128 {
    match width {
        1 => index,
        2 => 10 + index,
        _ => 100 + index,
    }
}

/// Every reserved form a dotted quad `len` characters long can take: the first
/// octet `0` or `240`–`255`, and the widths of the three others.
fn forms(len: usize) -> Vec<(bool, [usize; 3])> {
    let mut out = Vec::new();
    for wide_first in [false, true] {
        let rest = len.checked_sub(3 + if wide_first { 3 } else { 1 });
        for w in (1..=3).flat_map(|a| (1..=3).flat_map(move |b| (1..=3).map(move |c| [a, b, c]))) {
            if rest == Some(w.iter().sum()) {
                out.push((wide_first, w));
            }
        }
    }
    out
}

/// The fake of a dotted-quad address, of the same text width, drawn uniformly
/// over every reserved address that width can spell.
pub fn fake_ipv4(seed: &Seed, text: &str) -> Option<String> {
    ipv4_octets(text)?;
    let forms = forms(text.len());
    let size = |(wide, w): &(bool, [usize; 3])| {
        (if *wide { 16 } else { 1 }) * w.iter().map(|&x| values_of(x)).product::<u128>()
    };
    let total: u128 = forms.iter().map(size).sum();
    let hash = digest(seed, format!("ipv4\0{text}").as_bytes());
    let mut index = u128::from_be_bytes(hash[..16].try_into().expect("16 bytes")) % total;
    for form in &forms {
        if index >= size(form) {
            index -= size(form);
            continue;
        }
        let (wide, widths) = form;
        let mut parts = Vec::with_capacity(4);
        if *wide {
            parts.push((240 + index % 16).to_string());
            index /= 16;
        } else {
            parts.push("0".to_string());
        }
        for &w in widths {
            parts.push(octet(w, index % values_of(w)).to_string());
            index /= values_of(w);
        }
        return Some(parts.join("."));
    }
    None
}

/// The fake of an IPv6 address as written, of the same text: every hex digit
/// hashed in place (its case kept) and the first group opening on `fd`; an
/// IPv4-mapped address keeps its prefix and takes [`fake_ipv4`] of its IPv4
/// part. `None` when the text is no address or its first group is narrower
/// than four digits.
pub fn fake_ipv6(seed: &Seed, text: &str) -> Option<String> {
    text.parse::<Ipv6Addr>().ok()?;
    if let Some((prefix, v4)) = text.rsplit_once(':').filter(|(_, v4)| v4.contains('.')) {
        return Some(format!("{prefix}:{}", fake_ipv4(seed, v4)?));
    }
    let first = text.split(':').next()?;
    if first.len() != 4 {
        return None;
    }
    let hash = digest(seed, format!("ipv6\0{}", text.to_ascii_lowercase()).as_bytes());
    let hex = b"0123456789abcdef";
    let out: String = text
        .bytes()
        .enumerate()
        .map(|(i, b)| {
            let digit = match i {
                0 => b'f',
                1 => b'd',
                _ => hex[usize::from(hash[i % 32] ^ (i / 32) as u8) % 16],
            };
            match b {
                b'0'..=b'9' | b'a'..=b'f' => char::from(digit),
                b'A'..=b'F' => char::from(digit.to_ascii_uppercase()),
                other => char::from(other),
            }
        })
        .collect();
    Some(out)
}

#[cfg(test)]
mod tests;
