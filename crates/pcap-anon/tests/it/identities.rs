//! The identities a flows document holds, read by the tests from its
//! messages, as the crate's README states what the anonymiser rewrites: a
//! `sip:`/`sips:`/`tel:` user part of four digits or more, a user part that
//! is a name (two characters or more, a letter among them), a host name that
//! is no special-use name, and a public address that is no SUT address.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde_json::Value;

/// The bytes that end a URI's user part or host.
const URI_END: &[char] = &[';', '>', '?', '"', ' ', '\r', '\n', ',', '\t'];

/// The special-use names (RFC 6761, RFC 2606) the anonymiser keeps.
fn special_use(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    let last = lower.rsplit('.').next().unwrap_or_default();
    lower == "localhost"
        || matches!(last, "invalid" | "test" | "example" | "localhost")
        || ["example.com", "example.net", "example.org"]
            .iter()
            .any(|d| lower == *d || lower.ends_with(&format!(".{d}")))
}

/// Whether `addr` is an address a peer holds on the public network: none of
/// the private, loopback, link-local, shared, documentation or reserved ranges.
fn public(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            let documentation = matches!((a, b, c), (192, 0, 2) | (198, 51, 100) | (203, 0, 113));
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || documentation
                || (a == 100 && (64..128).contains(&b))
                || a == 0
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            let [first, second, ..] = v6.segments();
            let documentation =
                (first == 0x2001 && second == 0x0db8) || (first == 0x3fff && second < 0x1000);
            (first & 0xe000) == 0x2000 && !documentation
        }
    }
}

/// The address of `host`, written bare, with a port, or bracketed.
fn address(host: &str) -> Option<IpAddr> {
    let bare = host.trim_start_matches('[');
    let bare = bare.split(']').next().unwrap_or(bare);
    if let Ok(v6) = bare.parse::<Ipv6Addr>() {
        return Some(IpAddr::V6(v6));
    }
    bare.split(':').next().and_then(|h| h.parse::<Ipv4Addr>().ok()).map(IpAddr::V4)
}

/// The identities of a host as written: its public address, or its name.
fn host_identity(host: &str, sut: &[IpAddr], out: &mut BTreeSet<String>) {
    match address(host) {
        Some(addr) if public(addr) && !sut.contains(&addr) => {
            out.insert(addr.to_string());
        }
        Some(_) => {}
        None => {
            let name = host.split(':').next().unwrap_or(host);
            if name.contains(|c: char| c.is_ascii_alphabetic()) && !special_use(name) {
                out.insert(name.to_ascii_lowercase());
            }
        }
    }
}

/// The identity of a user part: a number of four digits or more (its `+`
/// left out), or a name of two characters or more.
fn user_identity(user: &str, out: &mut BTreeSet<String>) {
    let digits = user.trim_start_matches('+');
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        if digits.len() >= 4 {
            out.insert(digits.to_string());
        }
    } else if user.len() >= 2 && user.contains(|c: char| c.is_ascii_alphabetic()) {
        out.insert(user.to_ascii_lowercase());
    }
}

/// The identities of every URI of `text`.
fn uri_identities(text: &str, sut: &[IpAddr], out: &mut BTreeSet<String>) {
    for scheme in ["sip:", "sips:", "tel:"] {
        for (at, _) in text.match_indices(scheme) {
            if text[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            let rest = &text[at + scheme.len()..];
            let uri = rest.split(URI_END).next().unwrap_or_default();
            match (scheme, uri.split_once('@')) {
                ("tel:", _) => user_identity(uri, out),
                (_, Some((user, host))) => {
                    user_identity(user, out);
                    host_identity(host, sut, out);
                }
                (_, None) => host_identity(uri, sut, out),
            }
        }
    }
}

/// The identities a flows document holds: those of its messages' URIs and
/// of their sockets, the `sut` addresses left out.
pub fn of_document(doc: &Value, sut: &[IpAddr]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for msg in messages(doc) {
        for socket in ["src", "dst"] {
            if let Some(socket) = msg[socket].as_str() {
                host_identity(socket, sut, &mut out);
            }
        }
        uri_identities(msg["raw"].as_str().unwrap_or_default(), sut, &mut out);
    }
    out
}

/// Every message of the document's legs.
pub fn messages(doc: &Value) -> impl Iterator<Item = &Value> {
    doc["legs"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|leg| leg["msgs"].as_array().into_iter().flatten())
}

/// Whether `text` holds `word` whole, its case ignored: a match whose
/// neighbours are no letter or digit.
pub fn holds_word(text: &str, word: &str) -> bool {
    let (text, word) = (text.to_ascii_lowercase(), word.to_ascii_lowercase());
    let bytes = text.as_bytes();
    text.match_indices(&word).any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + word.len()).copied();
        !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && !after.is_some_and(|b| b.is_ascii_alphanumeric())
    })
}

/// Every string value of the document, keys left out, one per line.
pub fn strings(doc: &Value) -> String {
    let mut out = String::new();
    let mut stack = vec![doc];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.values()),
            _ => {}
        }
    }
    out
}
