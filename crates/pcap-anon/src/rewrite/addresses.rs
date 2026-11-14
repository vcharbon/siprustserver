//! Address literals in a datagram or a model string, rewritten through the
//! caller's mapping: every dotted quad and every IPv6 address that stands
//! alone — no letter, digit, dot or colon on either side, a dot behind a
//! letter or a percent escape (`%40`) aside, so an address in
//! brackets, behind `received=`, in an SDP `c=` or ICE `candidate` line, or in
//! any header value is found. A run of digits and dots that is not exactly
//! four octets — a version number, a date — is no address, and neither is a
//! run of hex digits and colons that does not parse as one (a time, a
//! fingerprint).

use crate::fake::address::ipv4_octets;

/// One literal: its byte span in the input.
pub type Span = (usize, usize);

/// Whether `b` may continue an address literal.
fn literal_byte(b: u8) -> bool {
    b.is_ascii_hexdigit() || b == b':' || b == b'.'
}

/// Every address literal of `input`, in input order.
pub fn literals(input: &[u8]) -> Vec<Span> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < input.len() {
        // A colon before the run is a scheme's or a parameter's (`sip:`), unless
        // what precedes it would itself continue an address.
        let escaped_before = i >= 3
            && input[i - 3] == b'%'
            && input[i - 2].is_ascii_hexdigit()
            && input[i - 1].is_ascii_hexdigit();
        let starts = literal_byte(input[i])
            && (i == 0
                || escaped_before
                || match input[i - 1] {
                    b'%' => false,
                    b':' => i < 2 || !literal_byte(input[i - 2]) || behind_word(input, i - 1),
                    // A dot behind a letter ends a token (`z9hG4bK.192.0.2.1`).
                    b'.' => i >= 2 && input[i - 2].is_ascii_alphabetic(),
                    b => !b.is_ascii_alphanumeric(),
                });
        if !starts {
            i += 1;
            continue;
        }
        let end = i + input[i..].iter().take_while(|&&b| literal_byte(b)).count();
        let bounded = input.get(end).is_none_or(|b| !b.is_ascii_alphanumeric());
        let text = std::str::from_utf8(&input[i..end]).expect("ASCII");
        if let Some(len) = bounded.then(|| address_len(text)).flatten() {
            found.push((i, i + len));
        }
        i = end;
    }
    found
}

/// Whether the colon at `colon` ends a word rather than an address: the
/// literal bytes before it (`cname` spells `e` in hex) open behind a letter
/// or digit that cannot continue an address (`cname:192.0.2.1`).
fn behind_word(input: &[u8], colon: usize) -> bool {
    let start = input[..colon].iter().rposition(|&b| !literal_byte(b)).map_or(0, |p| p + 1);
    start > 0 && input[start - 1].is_ascii_alphanumeric()
}

/// The length of the address a run of literal bytes opens with: the whole run
/// as an IPv6 address (a trailing `:` or `.` aside), else the dotted quad
/// before a `:port`, else none.
fn address_len(run: &str) -> Option<usize> {
    let trimmed = run.trim_end_matches([':', '.']);
    if trimmed.contains(':') && trimmed.parse::<std::net::Ipv6Addr>().is_ok() {
        return Some(trimmed.len());
    }
    let head = run.split(':').next().unwrap_or_default();
    let quad = if ipv4_octets(head).is_some() { head } else { trimmed };
    ipv4_octets(quad).map(|_| quad.len())
}

/// `input` with every literal `map` answers for replaced by its answer.
pub fn rewrite(input: &[u8], map: &dyn Fn(&str) -> Option<String>) -> Vec<u8> {
    let spans = literals(input);
    if spans.is_empty() {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len());
    let mut at = 0;
    for (start, end) in spans {
        let text = std::str::from_utf8(&input[start..end]).expect("an ASCII literal");
        out.extend_from_slice(&input[at..start]);
        match map(text) {
            Some(fake) => out.extend_from_slice(fake.as_bytes()),
            None => out.extend_from_slice(&input[start..end]),
        }
        at = end;
    }
    out.extend_from_slice(&input[at..]);
    out
}

#[cfg(test)]
mod tests;
