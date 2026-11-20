//! The fakes of the identities that are words rather than numbers: host
//! names and realms, product and free text, charging ids. Each keeps the byte
//! length of what it replaces and is a pure function of (seed, value), keyed
//! by the lowercase spelling; the substitution gives each occurrence the case
//! it was written in.
//!
//! A host or a text is faked word by word — a word is a run of ASCII letters,
//! digits and non-ASCII bytes — so a domain maps the same under every host
//! that ends in it: every letter (a non-ASCII byte included) becomes a hashed
//! letter `a`–`z` and every digit a hashed digit, so two distinct words of
//! one length rarely share a fake, and no word is its own fake. A percent
//! escape is no part of a word. A dotted quad inside a name becomes a
//! reserved address ([`crate::fake::address::fake_ipv4`]). A charging id is
//! faked whole behind `zx`. Under a capture's number keys (the `_among`
//! forms) no fake writes one of them as a whole digit run: hashed digits that
//! spelled a short number would leave it in clear, so that draw is drawn
//! again. Nothing here tells a fake from a real value: the
//! anonymiser runs on raw documents only, and the leak check judges its
//! output against the fakes the run made ([`crate::anonymize::Fakes`]).

use std::collections::HashSet;

use crate::fake::address::{fake_ipv4, ipv4_octets, is_reserved_v4};
use crate::fake::{digest, Seed};

/// The fake of a host name, a realm or an inter-operator id.
pub fn fake_host(seed: &Seed, host: &str) -> String {
    fake_host_among(seed, host, &HashSet::new())
}

/// [`fake_host`] in a capture whose number keys are `numbers`: no fake
/// writes one of them as a whole digit run.
pub fn fake_host_among(seed: &Seed, host: &str, numbers: &HashSet<Vec<u8>>) -> String {
    fake_words(seed, "host", host, numbers)
}

/// The fake of a product string (`Server`, `User-Agent`) or of free text that
/// names an organisation, a network, a session or a user.
pub fn fake_text(seed: &Seed, text: &str) -> String {
    fake_text_among(seed, text, &HashSet::new())
}

/// [`fake_text`] in a capture whose number keys are `numbers`: no fake
/// writes one of them as a whole digit run.
pub fn fake_text_among(seed: &Seed, text: &str, numbers: &HashSet<Vec<u8>>) -> String {
    fake_words(seed, "text", text, numbers)
}

/// Whether a value is SIP or SDP vocabulary — a method, a transport, a
/// parameter name or value, an SDP token — or a single character, which a
/// substitution must never rewrite where it stands elsewhere in the datagram.
pub fn vocabulary(value: &str) -> bool {
    const WORDS: &[&str] = &[
        "sip",
        "sips",
        "tel",
        "user",
        "phone",
        "ip",
        "udp",
        "tcp",
        "tls",
        "sctp",
        "ws",
        "wss",
        "in",
        "ip4",
        "ip6",
        "rtp",
        "avp",
        "savp",
        "audio",
        "video",
        "session",
        "call",
        "sdp",
        "invite",
        "ack",
        "bye",
        "cancel",
        "options",
        "info",
        "update",
        "prack",
        "refer",
        "notify",
        "message",
        "subscribe",
        "register",
        "publish",
        "media",
        "default",
        "none",
        "anonymous",
        "unknown",
        "lr",
        "ob",
        "gr",
        "transport",
        "maddr",
        "ttl",
        "method",
        "tag",
        "branch",
        "received",
        "rport",
        "expires",
        "q",
        "true",
        "false",
        "yes",
        "no",
        "on",
        "off",
    ];
    // One character names no one, and as a pattern it would match everywhere.
    value.trim().len() < 2 || WORDS.iter().any(|w| w.eq_ignore_ascii_case(value.trim()))
}

/// Whether a host name stays as found: vocabulary, an IPv4 address (the
/// address pass owns it), no host name at all (an IPv6 reference, a misread
/// user part), or a special-use name (RFC 6761: `invalid`,
/// `test`, `example`, `localhost`), which names no one.
pub fn keeps_host(host: &str) -> bool {
    if vocabulary(host) {
        return true;
    }
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = lower.split('.').collect();
    let last = labels.last().copied().unwrap_or_default();
    lower.is_empty()
        || !lower.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        || ipv4_octets(&lower).is_some()
        || (labels.len() > 1 && matches!(last, "invalid" | "test" | "example" | "localhost"))
        || lower == "localhost"
        || ["example.com", "example.net", "example.org"]
            .iter()
            .any(|d| lower == *d || lower.ends_with(&format!(".{d}")))
}

/// Whether free text stays as found: it holds no letter — its digits are
/// the number pass's ([`crate::identities::harvest`] hands them over) — or
/// is vocabulary.
pub fn keeps_text(text: &str) -> bool {
    !text.bytes().any(|b| b.is_ascii_alphabetic() || b >= 0x80) || vocabulary(text)
}

/// A same-length redaction: every letter, digit and `-` (the base64url
/// alphabet) becomes `_`, every other byte stays, so the value keeps its
/// shape and length and carries nothing of what it held — not even the case
/// of its letters, which a substitution would copy onto a letter.
pub fn redact(value: &str) -> String {
    value.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { '_' } else { c }).collect()
}

/// The fake of a charging id (`icid-value`): `zx`, then every letter (or
/// non-ASCII byte) a hashed letter and every digit a hashed digit,
/// punctuation kept.
pub fn fake_icid(seed: &Seed, icid: &str) -> String {
    fake_icid_among(seed, icid, &HashSet::new())
}

/// [`fake_icid`] in a capture whose number keys are `numbers`: a fake that
/// writes one of them as a whole digit run is drawn again, under a round
/// number.
pub fn fake_icid_among(seed: &Seed, icid: &str, numbers: &HashSet<Vec<u8>>) -> String {
    let lower = icid.to_ascii_lowercase();
    let draw = |class: &str| -> String {
        let hash = hash_bytes(seed, class, lower.as_bytes(), icid.len());
        lower
            .bytes()
            .zip(hash)
            .enumerate()
            .map(|(i, (b, h))| match i {
                0 => 'z',
                1 => 'x',
                _ => char::from(fake_byte(b, h).unwrap_or(b)),
            })
            .collect()
    };
    let mut fake = draw("icid");
    let mut round = 0;
    while writes_a_number(fake.as_bytes(), numbers) {
        round += 1;
        fake = draw(&format!("icid!{round}"));
    }
    fake
}

/// Whether a whole digit run of `fake` is one of `numbers`.
fn writes_a_number(fake: &[u8], numbers: &HashSet<Vec<u8>>) -> bool {
    !numbers.is_empty()
        && fake.split(|b| !b.is_ascii_digit()).any(|run| !run.is_empty() && numbers.contains(run))
}

/// Whether a charging id stays as found: too short to tell one charging
/// session from another.
pub fn keeps_icid(icid: &str) -> bool {
    icid.len() < 4
}

/// One piece of a name: a dotted quad, a word, or a byte between words.
enum Piece<'a> {
    Quad(&'a str),
    Word(&'a [u8]),
    Other(u8),
}

/// A byte of a word: an ASCII letter or digit, or any non-ASCII byte.
fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// `bytes` cut into its pieces, in order. A percent escape (`%40`, RFC 3261
/// §19.1.2) spells one byte: its three bytes stay as written and end the
/// word before them.
fn walk<'a>(bytes: &'a [u8], mut each: impl FnMut(Piece<'a>)) {
    let quads = crate::rewrite::addresses::literals(bytes);
    let mut i = 0;
    while i < bytes.len() {
        if let Some(&(_, end)) = quads.iter().find(|(s, _)| *s == i) {
            let quad = std::str::from_utf8(&bytes[i..end]).expect("an ASCII literal");
            if ipv4_octets(quad).is_some() {
                each(Piece::Quad(quad));
                i = end;
                continue;
            }
        }
        if bytes[i] == b'%'
            && bytes.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
        {
            bytes[i..i + 3].iter().for_each(|&b| each(Piece::Other(b)));
            i += 3;
            continue;
        }
        if !word_byte(bytes[i]) {
            each(Piece::Other(bytes[i]));
            i += 1;
            continue;
        }
        let end = i + bytes[i..].iter().take_while(|&&b| word_byte(b)).count();
        each(Piece::Word(&bytes[i..end]));
        i = end;
    }
}

/// [`fake_host_among`] and [`fake_text_among`]: every piece faked in place.
fn fake_words(seed: &Seed, class: &str, text: &str, numbers: &HashSet<Vec<u8>>) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    walk(text.as_bytes(), |piece| match piece {
        Piece::Quad(quad) => {
            let reserved = ipv4_octets(quad).is_some_and(|(o, _)| is_reserved_v4(o.into()));
            let fake = if reserved { None } else { fake_ipv4(seed, quad) };
            out.extend_from_slice(fake.as_deref().unwrap_or(quad).as_bytes());
        }
        Piece::Word(word) => out.extend(fake_word(seed, class, word, numbers)),
        Piece::Other(b) => out.push(b),
    });
    no_new_addresses(text.as_bytes(), &mut out);
    String::from_utf8(out).expect("ASCII words between the input's own non-word bytes")
}

/// A dotted run of four numbers that is no address (a version, `12.3.456.7`)
/// is faked label by label, which may spell one: its first three-digit label
/// then opens on a digit from `3` up, so the fake never reads as an address
/// the input did not hold.
fn no_new_addresses(input: &[u8], out: &mut [u8]) {
    let dotted = |b: &u8| b.is_ascii_digit() || *b == b'.';
    let mut i = 0;
    while i < input.len() {
        if !dotted(&input[i]) {
            i += 1;
            continue;
        }
        let run_end = i + input[i..].iter().take_while(|b| dotted(b)).count();
        // The run without the dots that open or close it, as an address reads.
        let start = i + input[i..run_end].iter().take_while(|b| **b == b'.').count();
        let end = run_end - input[start..run_end].iter().rev().take_while(|b| **b == b'.').count();
        let quad = |b: &[u8]| ipv4_octets(&String::from_utf8_lossy(b)).is_some();
        if !quad(&input[start..end]) && quad(&out[start..end]) {
            let mut at = start;
            for label in out[start..end].split(|b| *b == b'.') {
                if label.len() == 3 {
                    break;
                }
                at += label.len() + 1;
            }
            if at < end {
                out[at] = b'3' + (out[at] - b'0') % 7;
            }
        }
        i = run_end;
    }
}

/// One word's fake, lowercase: every byte hashed in its class.
/// A fake that spells its own word, or writes one of `numbers` as a whole
/// digit run, is drawn again, under a round number, so no word is left as it
/// was and none spells a number of the capture. A word's digit run stands
/// whole in the text too, unless a percent escape's hex digits touch it.
fn fake_word(seed: &Seed, class: &str, word: &[u8], numbers: &HashSet<Vec<u8>>) -> Vec<u8> {
    let lower = word.to_ascii_lowercase();
    let draw = |class: &str| -> Vec<u8> {
        let hash = hash_bytes(seed, class, &lower, word.len());
        lower.iter().zip(hash).map(|(&b, h)| fake_byte(b, h).unwrap_or(b'x')).collect()
    };
    let mut fake = draw(class);
    let mut round = 0;
    while fake == lower || writes_a_number(&fake, numbers) {
        round += 1;
        fake = draw(&format!("{class}!{round}"));
    }
    fake
}

/// `len` hash bytes of (seed, class, value): the HMAC of the value, then of
/// the value with each further block's number.
fn hash_bytes(seed: &Seed, class: &str, value: &[u8], len: usize) -> Vec<u8> {
    let message = |block: usize| {
        let mut m = format!("{class}\0").into_bytes();
        m.extend_from_slice(value);
        if block > 0 {
            m.extend_from_slice(format!("#{block}").as_bytes());
        }
        m
    };
    let mut out = Vec::with_capacity(len.max(32));
    let mut block = 0;
    while out.len() < len {
        out.extend(digest(seed, &message(block)));
        block += 1;
    }
    out
}

/// One word byte's fake under hash byte `h`: a letter (a non-ASCII byte
/// included) a hashed letter, a digit a hashed digit; `None` for any other
/// byte.
fn fake_byte(b: u8, h: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b'0' + h % 10),
        b if b.is_ascii_alphabetic() || b >= 0x80 => Some(b'a' + h % 26),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
