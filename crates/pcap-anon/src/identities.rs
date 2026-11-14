//! A datagram's identity surface, read through `sip-message`: [`harvest`]
//! collects the R-URI, every header whose value carries a `sip:`/`sips:`/
//! `tel:` URI (the name-addr headers under their own kind, every other header
//! by a URI scan of its value), the PASSporT payload of every `Identity` header and the
//! payload of every User-to-User (header or escaped URI header), and the
//! names outside the numbers ([`names`]: hosts, charging ids, product text) —
//! and exposes the few whole-message probes the anonymizer needs for its
//! numeric-field guard and parse smoke ([`cseq_seq`], [`call_id`],
//! [`parses`]). The anonymizer's rewrite itself is a plain byte-substring
//! pass. `urn:` identities yield no number (frozen by construction);
//! classification/`anonymous` policy belongs to the caller.

use sip_message::header::{kind, HeaderName};
use sip_message::parser::custom::structured_headers::split_top_level_commas;
use sip_message::{sniff, SipMessage, SipStr};

use crate::reading::Reading;

pub mod bodies;
pub mod body_types;
mod digits;

pub use body_types::BodyTypes;

pub use digits::PHONE_WIDTH;
pub mod names;
mod uris;

use uris::{harvest_line, push_uri, scan_uris};

/// What one datagram's identity surface contains, in wire order.
#[derive(Debug, Default)]
pub struct Harvested {
    /// Number-bearing user-parts: sip/sips URI users and tel numbers,
    /// verbatim (junk suffixes included).
    pub numbers: Vec<String>,
    /// Display names, as the bytes the datagram carries them in, a quoted one
    /// between its quotes with its escapes (frozen-name policy is the
    /// caller's).
    pub displays: Vec<Vec<u8>>,
    /// The JWS payload segment (base64url, verbatim) of every `Identity`
    /// header carrying a compact PASSporT (`header.payload.signature`).
    pub passports: Vec<String>,
    /// Every User-to-User payload (RFC 7433), verbatim, from the header and
    /// from the escaped `?User-to-User=` header of any harvested URI.
    pub uuis: Vec<Uui>,
    /// Every digit run no harvest above reads as a number and no protocol
    /// counter ([`digits`]): the number pass maps them too.
    pub digit_runs: Vec<String>,
    /// Number-bearing user-parts as written with visual separators
    /// (`+28-61-21-21-21`, RFC 3966 §5.1.1) or escapes (`%2B%328…`, RFC
    /// 3261 §19.1.2); `numbers` holds them plain.
    pub separated: Vec<String>,
    /// The names outside the numbers: every sip/sips URI host, and what
    /// [`names::harvest`] reads.
    pub names: names::Names,
}

/// One User-to-User payload and what its `encoding` parameter says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uui {
    pub payload: String,
    pub encoding: UuiEncoding,
}

/// How a User-to-User payload is read: `hex` decodes to bytes; `ascii` or
/// `text` is the bytes themselves; `base64` that decodes to text
/// ([`crate::encoded`]) is that text; anything else (absent, an unknown
/// token) is counted by the anonymizer, not guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UuiEncoding {
    Hex,
    Text,
    Base64,
    Other,
}

impl Uui {
    /// Every maximal digit run the payload carries as text: a hex payload's
    /// decoded, a text payload's own, those of the text a base64 payload
    /// decodes to. `None` when the payload does not read under its encoding.
    pub fn digit_runs(&self) -> Option<Vec<String>> {
        match self.encoding {
            UuiEncoding::Hex => {
                crate::uui::decode(&self.payload).map(|_| crate::uui::digit_runs(&self.payload))
            }
            UuiEncoding::Text => Some(crate::uui::text_digit_runs(&self.payload)),
            UuiEncoding::Base64 => crate::encoded::Text::of(&self.payload)
                .map(|t| crate::uui::text_digit_runs(&String::from_utf8_lossy(&t.text))),
            UuiEncoding::Other => None,
        }
    }
}

/// The payload and encoding of a User-to-User value (`payload;encoding=hex;…`,
/// the header form or the percent-decoded URI-header form).
fn uui_of(value: &str) -> Option<Uui> {
    let mut items = split_top_level_commas(value).into_iter().next()?.split(';');
    let payload = items.next()?.trim();
    if payload.is_empty() {
        return None;
    }
    let encoding = items.find_map(|param| {
        let (key, value) = param.trim().split_once('=')?;
        key.trim().eq_ignore_ascii_case("encoding").then(|| value.trim().to_ascii_lowercase())
    });
    let encoding = match encoding.as_deref() {
        Some("hex") => UuiEncoding::Hex,
        Some("ascii" | "text") => UuiEncoding::Text,
        Some("base64") => UuiEncoding::Base64,
        _ => UuiEncoding::Other,
    };
    Some(Uui { payload: payload.to_string(), encoding })
}

/// Harvest one datagram. A datagram that is not UTF-8 — a display name
/// spelling a character in CESU-8, a Latin-1 byte — is read one character per
/// byte, its SIP structure being ASCII, and its display names handed back as
/// the wire bytes they were read from. Unparseable datagrams harvest nothing —
/// the anonymizer's parse-smoke guard reports them; discovery just moves on.
pub fn harvest(raw: &[u8], types: &BodyTypes) -> Harvested {
    if std::str::from_utf8(raw).is_ok() {
        return harvest_text(raw, types, |display| display.as_bytes().to_vec());
    }
    let text: String = raw.iter().map(|&b| char::from(b)).collect();
    harvest_text(text.as_bytes(), types, |display| {
        display.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect()
    })
}

/// [`harvest`] over a datagram's text, `wire` turning a display name read
/// off that text back into the bytes the datagram carries.
fn harvest_text(raw: &[u8], types: &BodyTypes, wire: impl Fn(&str) -> Vec<u8>) -> Harvested {
    let mut out = Harvested::default();
    // The lenient reading: a datagram off the wire grammar still gives up its
    // identities, so a shape a gate refuses leaks none.
    let Ok(msg) = sip_message::read_stored(raw) else {
        out.digit_runs = digits::runs(None, raw, types, &[], &[]);
        return out;
    };
    // A reason phrase is protocol text, never read for names.
    if let SipMessage::Request(req) = &msg {
        push_uri(&mut out, req.request_uri());
    }
    harvest_headers(msg.headers(), &mut out);
    bodies::harvest(&msg, raw, types, &mut out);
    out.displays = std::mem::take(&mut out.displays)
        .into_iter()
        .map(|d| std::str::from_utf8(&d).map_or_else(|_| d.clone(), &wire))
        .collect();
    let payloads: Vec<&[u8]> = out
        .uuis
        .iter()
        .map(|u| u.payload.as_bytes())
        .chain(out.passports.iter().map(String::as_bytes))
        .collect();
    let displays: Vec<&[u8]> = out.displays.iter().map(Vec::as_slice).collect();
    out.digit_runs = digits::runs(Some(&msg), raw, types, &payloads, &displays);
    // A run a number above already starts with is that number's.
    let leading = |n: &String| {
        let n = n.strip_prefix('+').unwrap_or(n);
        n[..n.bytes().take_while(u8::is_ascii_digit).count()].to_string()
    };
    let read: std::collections::HashSet<String> = out.numbers.iter().map(leading).collect();
    out.digit_runs.retain(|run| !read.contains(run));
    numbers_of_letterless_texts(&mut out);
    out
}

/// Harvest a header block — a message's, a sipfrag's, a MIME part's: its
/// names ([`names::harvest`]), the name-addr identity headers each read under
/// its own kind so that entry boundaries follow that header's folding rule,
/// and every other header (vendor `P-*`, unknown names, the route set) by a
/// URI scan of its value; a vendor header's quoted strings are display
/// names, masked like them, and its non-ASCII words free text.
fn harvest_headers(headers: &[sip_message::SipHeader], out: &mut Harvested) {
    names::harvest(headers, &mut out.names);
    for h in headers {
        if let HeaderName::Other(name) = HeaderName::of(&h.name) {
            if !names::vocabulary_header(name.as_str()) {
                vendor(h.value.as_str(), out);
            }
        }
        match HeaderName::of(&h.name) {
            HeaderName::From => harvest_line::<kind::From>(&h.value, out),
            HeaderName::To => harvest_line::<kind::To>(&h.value, out),
            HeaderName::Contact => harvest_line::<kind::Contact>(&h.value, out),
            HeaderName::PAssertedIdentity => harvest_line::<kind::PAssertedIdentity>(&h.value, out),
            HeaderName::PPreferredIdentity => {
                harvest_line::<kind::PPreferredIdentity>(&h.value, out)
            }
            HeaderName::Diversion => harvest_line::<kind::Diversion>(&h.value, out),
            HeaderName::RemotePartyId => harvest_line::<kind::RemotePartyId>(&h.value, out),
            HeaderName::HistoryInfo => harvest_line::<kind::HistoryInfo>(&h.value, out),
            HeaderName::ReferTo => harvest_line::<kind::ReferTo>(&h.value, out),
            HeaderName::ReferredBy => harvest_line::<kind::ReferredBy>(&h.value, out),
            _ if h.name.as_str().eq_ignore_ascii_case("Identity") => {
                if let Some(payload) = passport_payload(h.value.as_str()) {
                    out.passports.push(payload.to_string());
                }
            }
            _ if h.name.as_str().eq_ignore_ascii_case("User-to-User") => {
                out.uuis.extend(uui_of(h.value.as_str()));
            }
            // A Reason's or a Warning's text is protocol text, never read.
            HeaderName::Reason | HeaderName::Warning => {
                scan_uris(&SipStr::owned(&names::outside_quotes(h.value.as_str())), out)
            }
            _ => scan_uris(&h.value, out),
        }
    }
}

/// A vendor header's quoted strings that are no parameter value, as display
/// names, and its non-ASCII words, as text.
fn vendor(value: &str, out: &mut Harvested) {
    for quoted in quoted_strings(value) {
        out.displays.push(quoted.as_bytes().to_vec());
    }
    let words = value.split(|c: char| c.is_ascii_whitespace() || "\",;<>=".contains(c));
    out.names.texts.extend(words.filter(|w| !w.is_ascii()).map(str::to_string));
}

/// The quoted strings of a header value that are no parameter value (`a="…"`).
pub(crate) fn quoted_strings(value: &str) -> Vec<&str> {
    quoted_all(value).into_iter().filter(|(parameter, _)| !parameter).map(|(_, q)| q).collect()
}

/// Every non-empty quoted string of a header value, its escapes (`\"`) kept
/// inside it (RFC 3261 §25.1 quoted-pair), with whether it is a parameter
/// value.
pub(crate) fn quoted_all(value: &str) -> Vec<(bool, &str)> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // A quote behind a backslash outside a quoted string opens none: it is
    // escaped data (`{\"key\":\"\"}`) a vendor writes.
    let opens = |p: usize| bytes[p] == b'"' && (p == 0 || bytes[p - 1] != b'\\');
    while let Some(open) = (i..bytes.len()).find(|&p| opens(p)) {
        let mut j = open + 1;
        while j < bytes.len() && bytes[j] != b'"' {
            j += if bytes[j] == b'\\' { 2 } else { 1 };
        }
        let close = j.min(bytes.len());
        let parameter = value[..open].trim_end().ends_with('=');
        if close > open + 1 {
            out.push((parameter, &value[open + 1..close]));
        }
        i = close + 1;
        if i >= bytes.len() {
            break;
        }
    }
    out
}

/// A text without a letter is a number written with separators (an SDP
/// `p=+28 6 12 …`) when its digits are wide enough to name someone: the
/// number pass maps it (`fake::names::keeps_text` leaves it).
fn numbers_of_letterless_texts(out: &mut Harvested) {
    let letterless: Vec<String> = out
        .names
        .texts
        .iter()
        .filter(|t| !t.bytes().any(|b| b.is_ascii_alphabetic() || b >= 0x80))
        .cloned()
        .collect();
    for text in letterless {
        let digits: String = text.chars().filter(char::is_ascii_digit).collect();
        if digits.len() >= crate::plan::MIN_KEY_WIDTH {
            if digits != text {
                out.separated.push(text);
            }
            out.numbers.push(digits);
        }
    }
}

/// The payload segment of an `Identity` value carrying a compact PASSporT
/// (RFC 8224 §4: `header.payload.signature` then `;info=…` parameters);
/// `None` for any other shape (the quoted RFC 4474 form has no payload).
fn passport_payload(value: &str) -> Option<&str> {
    let token = value.split(';').next()?.trim();
    let mut segments = token.split('.');
    let (head, payload, signature) = (segments.next()?, segments.next()?, segments.next()?);
    let jws = segments.next().is_none()
        && !head.is_empty()
        && !payload.is_empty()
        && !signature.is_empty()
        && token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    jws.then_some(payload)
}

/// The CSeq sequence number of a raw datagram (the anonymizer's numeric-field
/// guard: this field is never rewritten, so it must still agree after a pass).
pub fn cseq_seq(raw: &[u8]) -> Option<u32> {
    sniff::cseq_number(raw)
}

/// The Call-ID of a raw datagram (numeric-field guard companion to [`cseq_seq`]).
pub fn call_id(raw: &[u8]) -> Option<String> {
    sniff::call_id(raw)
}

/// Where byte `at` of a datagram sits: `start-line`, the name of the header
/// whose value holds it, or `body` (`sip_message::site`).
pub fn site_of(raw: &[u8], at: usize) -> String {
    match sip_message::site::site_at(raw, at) {
        sip_message::site::Site::StartLine => "start-line".to_string(),
        sip_message::site::Site::Header(name) => name,
        sip_message::site::Site::Body => "body".to_string(),
    }
}

/// Whether a raw datagram parses as SIP under `reading` — the parse-smoke
/// oracle: anonymization must not turn a parseable datagram into an
/// unparseable one.
pub fn parses(raw: &[u8], reading: &Reading) -> bool {
    reading.parse(raw).is_ok()
}

/// Every body part of a datagram with its media type and byte span
/// ([`bodies::parts_of`]), read leniently as [`harvest`] reads it; nothing for
/// a datagram that does not read.
pub fn body_parts(raw: &[u8]) -> Vec<bodies::BodyPart> {
    sip_message::read_stored(raw).map_or_else(|_| Vec::new(), |msg| bodies::parts_of(&msg, raw))
}

/// The byte ranges of a datagram's multipart body outside every part and
/// every delimiter line ([`bodies::outside_parts_of`]), read as
/// [`body_parts`] reads; nothing for a datagram that does not read.
pub fn outside_parts(raw: &[u8]) -> Vec<std::ops::Range<usize>> {
    sip_message::read_stored(raw)
        .map_or_else(|_| Vec::new(), |msg| bodies::outside_parts_of(&msg, raw))
}

#[cfg(test)]
mod tests;
