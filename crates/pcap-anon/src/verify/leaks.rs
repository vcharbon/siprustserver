//! The leak check that does not trust the harvest: it reads the anonymised
//! document alone and judges it by the fakes the run wrote
//! ([`crate::anonymize::Fakes`]). Every domain-shaped token, every address
//! the scope maps, every word of free text (`words`), every digit run that
//! is no protocol counter (`digits`) and every non-ASCII run, head and body
//! alike, is identity-shaped; one that is neither a fake of the run, nor
//! protocol vocabulary, nor a special-use name, nor an address the scope
//! keeps names what the raw document named, and the distinct ones are counted
//! — never shown. Escapes are decoded before the datagram is read (`%40` hides
//! no host), and so is base64 text that is beyond accident
//! ([`crate::encoded::Text::sure`]), read as the clear text of the place it
//! stands at; a shorter text random bytes may spell is read as written.
//! Nothing reads as a fake by its shape.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::Serialize;
use sip_message::param_codec::decode_param;

use crate::anonymize::Fakes;
use crate::fake::address::{ipv4_octets, Scope};
use crate::fake::names::{keeps_host, vocabulary};
use crate::model::Flows;
use crate::reading::Reading;

/// The distinct identity-shaped tokens of the anonymised document no fake
/// explains.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Leaks {
    pub domains: usize,
    pub addresses: usize,
    pub words: usize,
    pub non_ascii: usize,
    pub digits: usize,
}

impl Leaks {
    pub fn total(&self) -> usize {
        self.domains + self.addresses + self.words + self.non_ascii + self.digits
    }
}

/// The unexplained tokens of one document, by class, lowercase.
#[derive(Default)]
struct Tokens {
    domains: HashSet<Vec<u8>>,
    addresses: HashSet<Vec<u8>>,
    words: HashSet<Vec<u8>>,
    non_ascii: HashSet<Vec<u8>>,
    digits: HashSet<Vec<u8>>,
}

mod digits;
mod words;

/// What of `anonymised`, read under `reading`, the run's `fakes` do not
/// explain.
pub fn leaks(anonymised: &Flows, scope: &Scope, fakes: &Fakes, reading: &Reading) -> Leaks {
    let out = tokens(anonymised, scope, fakes, reading);
    Leaks {
        domains: out.domains.len(),
        addresses: out.addresses.len(),
        words: out.words.len(),
        non_ascii: out.non_ascii.len(),
        digits: out.digits.len(),
    }
}

/// Every distinct token of `anonymised` the run's `fakes` do not explain,
/// with its class (`domain`, `address`, `word`, `non-ascii`, `digits`),
/// lowercase — for
/// a check that locates them, never for a report that prints them.
pub fn unexplained(
    anonymised: &Flows,
    scope: &Scope,
    fakes: &Fakes,
    reading: &Reading,
) -> Vec<(&'static str, Vec<u8>)> {
    let out = tokens(anonymised, scope, fakes, reading);
    let classes = [
        ("domain", out.domains),
        ("address", out.addresses),
        ("word", out.words),
        ("non-ascii", out.non_ascii),
        ("digits", out.digits),
    ];
    let mut found: Vec<(&'static str, Vec<u8>)> = classes
        .into_iter()
        .flat_map(|(class, set)| set.into_iter().map(move |t| (class, t)))
        .collect();
    found.sort();
    found
}

/// The unexplained tokens of `anonymised`. Protocol text is never
/// anonymised, so never read. Where the datagram read with its base64 text
/// decoded does not parse, the free text is read off the datagram as
/// written.
fn tokens(anonymised: &Flows, scope: &Scope, fakes: &Fakes, reading: &Reading) -> Tokens {
    let cx = crate::encoded::Context::of(anonymised, scope.bodies());
    let mut out = Tokens::default();
    for leg in &anonymised.legs {
        for msg in &leg.msgs {
            let written = crate::model::wire(msg);
            let decoded = crate::encoded::read_sure(&written, &cx).map(|read| read.bytes);
            let raw =
                super::without_protocol_text(decoded.as_ref().unwrap_or(&written), scope.bodies());
            let unescaped = decode_param(&String::from_utf8_lossy(&raw));
            for bytes in [unescaped.as_bytes(), msg.src.as_bytes(), msg.dst.as_bytes()] {
                domains(bytes, fakes, &mut out.domains);
                addresses(bytes, scope, fakes, &mut out.addresses);
            }
            let unread = decoded.is_some() && reading.parse(&raw).is_err();
            let as_written = unread.then(|| super::without_protocol_text(&written, scope.bodies()));
            for piece in
                words::free_text(as_written.as_ref().unwrap_or(&raw), reading, scope.bodies())
                    .into_iter()
                    .filter(|p| !scope.keeps_text(p))
            {
                for word in words::of(&piece) {
                    if !word.iter().all(u8::is_ascii_digit) && !explained(&word, fakes) {
                        out.words.insert(word.to_ascii_lowercase());
                    }
                }
            }
            out.digits.extend(digits::unexplained(&raw, fakes));
            for run in raw.split(|b| b.is_ascii()).filter(|r| !r.is_empty()) {
                out.non_ascii.insert(run.to_vec());
            }
        }
    }
    out
}

/// Whether a word is explained: a fake of the run, vocabulary, or a mask or
/// redaction — `x`/`X`, and digits the masking zeroed or the number pass
/// wrote.
pub(super) fn explained(word: &[u8], fakes: &Fakes) -> bool {
    let masked = word.iter().all(|b| b.eq_ignore_ascii_case(&b'x') || b.is_ascii_digit())
        && word
            .split(|b| !b.is_ascii_digit())
            .all(|run| run.iter().all(|b| *b == b'0') || fakes.wrote_digits(run));
    fakes.contains(word) || vocabulary(&String::from_utf8_lossy(word)) || masked
}

/// Every domain-shaped token of `bytes` no fake explains: two labels or more
/// of letters, digits, hyphens and underscores, the last opening on a letter — whatever
/// the top-level domain. A media subtype (`application/vnd.a.b`), a media
/// feature tag (`+g.…`, `+sip.…`, its `+` written), an emergency call data
/// purpose (RFC 7852 `EmergencyCallData.…`), a feature tag of the `g.` or
/// `sip.` tree standing as a parameter name without its `+`, a header field
/// name, a `g.`/`sip.`/`3gpp.`/`vnd.` name in a header of package, feature or
/// service names, an XML name and a URN's
/// namespace string are vocabulary; a special-use name names no one; a token every word of which
/// (a name is faked word by word) is a fake of the run, vocabulary or a mask
/// is explained.
fn domains(bytes: &[u8], fakes: &Fakes, out: &mut HashSet<Vec<u8>>) {
    let token_byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_');
    let mut i = 0;
    while i < bytes.len() {
        if !token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let end = i + bytes[i..].iter().take_while(|&&b| token_byte(b)).count();
        let (start, run) = (i, &bytes[i..end]);
        i = end;
        let text = String::from_utf8_lossy(run).trim_matches(['.', '-', '_']).to_ascii_lowercase();
        let labels: Vec<&str> = text.split('.').collect();
        let shaped = labels.len() >= 2
            && labels.iter().all(|l| !l.is_empty())
            && labels.last().is_some_and(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()));
        // A feature tag of the global or SIP tree (RFC 3840 §9) standing as a
        // parameter name, its `+` left out.
        let feature_tag = matches!(labels[0], "g" | "sip")
            && start > 0
            && bytes[start - 1] == b';'
            && bytes.get(end).is_none_or(|b| matches!(b, b'=' | b';' | b'>' | b',' | b'\r'));
        // A package, feature or service name of the global, SIP, 3GPP or vendor tree
        // in a header whose values are such names.
        let tree_name = matches!(labels[0], "g" | "sip" | "3gpp" | "vnd")
            && crate::rewrite::fields::header_name(bytes, start).is_some_and(|name| {
                name != "identity" && crate::identities::names::vocabulary_header(&name)
            });
        let registered = labels[0] == "emergencycalldata" || feature_tag || tree_name;
        if !shaped
            || vocabulary_context(bytes, start)
            || registered
            || field_name(bytes, start, end)
        {
            continue;
        }
        let mut words = text.split(['.', '-', '_']).filter(|w| !w.is_empty());
        if keeps_host(&text) || words.all(|w| explained(w.as_bytes(), fakes)) {
            continue;
        }
        out.insert(text.into_bytes());
    }
}

/// Whether `bytes[start..end]` is a header field name: it opens its line and
/// a `:` follows it, behind optional whitespace (RFC 3261 §7.3.1; a token
/// may hold a `.`).
fn field_name(bytes: &[u8], start: usize, end: usize) -> bool {
    let opens_line = start == 0 || bytes[start - 1] == b'\n';
    let colon = bytes[end..].iter().find(|b| !matches!(b, b' ' | b'\t'));
    opens_line && colon == Some(&b':')
}

/// Whether the token at `start` stands where protocol vocabulary does: behind
/// a single `/` (a media subtype, a path, an XML end tag — a `//` opens a
/// host), a `+` (a feature tag, its `!` name tree included) or a `<` (an XML
/// start tag), on an SDP `a=extmap` line (an extension's URI), as a `branch`
/// or `tag` value (an opaque id the element minted, RFC 3261 §8.1.1.7,
/// §19.3, whose digit runs and addresses the other classes read), or inside
/// a `urn:`.
fn vocabulary_context(bytes: &[u8], start: usize) -> bool {
    // An SDP extension map names its extension by URI (RFC 8285): vocabulary.
    if crate::rewrite::fields::line_at(bytes, start).starts_with(b"a=extmap") {
        return true;
    }
    // A feature tag's name tree (`+g.poc!vendor.example.com`) is vocabulary.
    let tag_start = bytes[..start]
        .iter()
        .rposition(|b| b.is_ascii_whitespace() || matches!(b, b';' | b'<' | b','));
    let tag = &bytes[tag_start.map_or(0, |p| p + 1)..start];
    if tag.starts_with(b"+") && tag.ends_with(b"!") {
        return true;
    }
    let host = start >= 2 && &bytes[start - 2..start] == b"//";
    if start > 0 && matches!(bytes[start - 1], b'/' | b'+' | b'<') && !host {
        return true;
    }
    // An identifier is the identifier pass's, its host part (`…@host`) aside.
    let host = start > 0 && bytes[start - 1] == b'@';
    if !host && crate::rewrite::identifiers::is_identifier(bytes, start, start) {
        return true;
    }
    let word_start = bytes[..start]
        .iter()
        .rposition(|b| b.is_ascii_whitespace() || b"<\"=,;".contains(b))
        .map_or(0, |p| p + 1);
    bytes[word_start..].get(..4).is_some_and(|s| s.eq_ignore_ascii_case(b"urn:"))
}

/// Every address of `bytes` the scope maps and no fake explains: each run of
/// digits and dots that is a dotted quad, whatever touches it (a letter glued
/// to an address does not hide it), and each run of hex digits, colons and
/// dots that is an IPv6 address. A longer dotted run — a version number — is
/// no address.
fn addresses(bytes: &[u8], scope: &Scope, fakes: &Fakes, out: &mut HashSet<Vec<u8>>) {
    for run in bytes.split(|b| !(b.is_ascii_digit() || *b == b'.')) {
        let quad = String::from_utf8_lossy(run).trim_matches('.').to_string();
        if let Some((octets, _)) = ipv4_octets(&quad) {
            if scope.maps_v4(Ipv4Addr::from(octets)) && !fakes.contains(quad.as_bytes()) {
                out.insert(quad.into_bytes());
            }
        }
    }
    for run in bytes.split(|b| !(b.is_ascii_hexdigit() || *b == b':' || *b == b'.')) {
        let text = String::from_utf8_lossy(run).trim_matches([':', '.']).to_ascii_lowercase();
        let mapped = text.contains(':') && text.parse::<Ipv6Addr>().is_ok_and(|a| scope.maps_v6(a));
        if mapped && !fakes.contains(text.as_bytes()) {
            out.insert(text.into_bytes());
        }
    }
}

#[cfg(test)]
mod tests;
