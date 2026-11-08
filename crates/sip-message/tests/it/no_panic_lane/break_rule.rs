//! The probe's break-rule mode: an accepted message with one operator breaking
//! one rule the RFC states, whatever else the message holds, so a fuzzer varies
//! the base around the conformance suite's operators. Every broken message is
//! refused by the parser that accepted its base; under a generic exemption set,
//! a URI parameter repeated with an equal value is accepted where that set names
//! it and a different value is refused whatever it names.

use sip_message::header::HeaderName;
use sip_message::parser::custom::CustomParser;
use sip_message::parser::Exemptions;
use sip_message::{serialize, SipMessage, SipParser, SipParserLimits};

/// The URI parameter the probe's exemption set lets From and Contact repeat.
pub const REPEATED: &str = "x-dup";

/// Every generic exemption name, the URI repeats naming [`REPEATED`].
const EXEMPTIONS: &str = "repeat:from.uri:x-dup,repeat:contact.uri:x-dup,bare-user-semicolon,\
                          max-forwards:absent,options-body-no-type";

/// The parser of a deployment stating every generic exemption, at the default
/// bounds.
pub fn exempting() -> CustomParser {
    let exemptions = Exemptions::read(Some(EXEMPTIONS)).expect("every generic name reads");
    CustomParser::with_limits(SipParserLimits { exemptions, ..SipParserLimits::DEFAULT })
}

/// A line past this length is left alone: an operator's few bytes could carry
/// it over a bound, which would refuse it for another reason.
const ROOM: usize = 4096;

/// A message rendered one header per line, as [`serialize`] writes it.
struct Lines {
    start: Vec<u8>,
    headers: Vec<Vec<u8>>,
    body: Vec<u8>,
}

impl Lines {
    fn of(msg: &SipMessage) -> Option<Self> {
        let wire = serialize(msg);
        let head_end = wire.windows(4).position(|w| w == b"\r\n\r\n")?;
        let mut lines = wire[..head_end].split(|&b| b == b'\n').map(|l| {
            let line = l.strip_suffix(b"\r").unwrap_or(l);
            line.to_vec()
        });
        let start = lines.next()?;
        Some(Self { start, headers: lines.collect(), body: wire[head_end + 4..].to_vec() })
    }

    /// The index of the first line of `name`.
    fn first(&self, name: &HeaderName) -> Option<usize> {
        self.headers.iter().position(|line| {
            let colon = line.iter().position(|&b| b == b':').unwrap_or(line.len());
            std::str::from_utf8(&line[..colon]).is_ok_and(|n| name.matches(n.trim()))
        })
    }

    fn count(&self, name: &HeaderName) -> usize {
        self.headers
            .iter()
            .filter(|line| {
                let colon = line.iter().position(|&b| b == b':').unwrap_or(line.len());
                std::str::from_utf8(&line[..colon]).is_ok_and(|n| name.matches(n.trim()))
            })
            .count()
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = self.head();
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }

    fn head(&self) -> Vec<u8> {
        let mut out = self.start.clone();
        out.extend_from_slice(b"\r\n");
        for line in &self.headers {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// The message with `edit` applied to its first line of `name`.
    fn edited(&self, name: &HeaderName, edit: impl FnOnce(&[u8]) -> Vec<u8>) -> Option<Vec<u8>> {
        let at = self.first(name).filter(|&at| self.headers[at].len() < ROOM)?;
        let mut copy = Lines {
            start: self.start.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
        };
        copy.headers[at] = edit(&self.headers[at]);
        Some(copy.bytes())
    }

    /// The message with its first line of `name` written twice.
    fn doubled(&self, name: &HeaderName) -> Option<Vec<u8>> {
        let at = self.first(name)?;
        let mut copy = Lines {
            start: self.start.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
        };
        copy.headers.insert(at + 1, self.headers[at].clone());
        Some(copy.bytes())
    }
}

fn appended(line: &[u8], tail: &[u8]) -> Vec<u8> {
    [line, tail].concat()
}

/// `inserted` right after the colon that ends the line's name.
fn after_colon(line: &[u8], inserted: &[u8]) -> Vec<u8> {
    let colon = line.iter().position(|&b| b == b':').map_or(line.len(), |at| at + 1);
    [&line[..colon], inserted, &line[colon..]].concat()
}

/// `params` added to the bracketed `sip:` or `sips:` URI of an address line,
/// after its host and before its headers; `None` where the line brackets no such URI or its URI
/// already names [`REPEATED`].
fn with_uri_params(line: &[u8], params: &str) -> Option<Vec<u8>> {
    let colon = line.iter().position(|&b| b == b':')?;
    let mut quoted = false;
    let mut escaped = false;
    let mut open = None;
    for (i, &b) in line.iter().enumerate().skip(colon + 1) {
        match (quoted, escaped, b) {
            (true, true, _) => escaped = false,
            (true, false, b'\\') => escaped = true,
            (_, false, b'"') => quoted = !quoted,
            (false, _, b'<') => {
                open = Some(i);
                break;
            }
            _ => {}
        }
    }
    let open = open?;
    let close = open + line[open..].iter().position(|&b| b == b'>')?;
    let uri = std::str::from_utf8(&line[open + 1..close]).ok()?;
    let lower = uri.to_ascii_lowercase();
    if !(lower.starts_with("sip:") || lower.starts_with("sips:")) || lower.contains(REPEATED) {
        return None;
    }
    // A user part may hold `;` and `?`: the parameters follow the host.
    let host = uri.find('@').map_or(0, |at| at + 1);
    let end = open + 1 + host + uri[host..].find('?').unwrap_or(uri.len() - host);
    Some([&line[..end], params.as_bytes(), &line[end..]].concat())
}

/// Whether `msg` states one Contact, on one line, held to the grammar: a final
/// of 300 or more keeps its lenient Contact (ADR-0053 §5).
fn one_contact(msg: &SipMessage, lines: &Lines) -> bool {
    let lenient = matches!(msg, SipMessage::Response(r) if r.status() >= 300);
    !lenient && msg.contacts().as_slice().len() == 1 && lines.count(&HeaderName::Contact) == 1
}

/// Every message an operator breaks out of `msg`, each with the operator's
/// name.
fn broken(msg: &SipMessage, lines: &Lines) -> Vec<(&'static str, Vec<u8>)> {
    let mut all = Vec::new();
    let mut add = |operator, raw: Option<Vec<u8>>| {
        if let Some(raw) = raw {
            all.push((operator, raw));
        }
    };
    // RFC 3261 §20 states each of these once.
    for name in [HeaderName::From, HeaderName::To, HeaderName::CallId, HeaderName::CSeq] {
        add("repeat-single-valued", lines.doubled(&name));
    }
    // §7.3.1: a parameter name appears once.
    if let Some(tag) = msg.from().tag() {
        let other = format!(";tag={tag}x");
        add("repeat-from-tag", lines.edited(&HeaderName::From, |l| appended(l, other.as_bytes())));
    }
    if let Some(tag) = msg.to().tag() {
        let other = format!(";tag={tag}x");
        add("repeat-to-tag", lines.edited(&HeaderName::To, |l| appended(l, other.as_bytes())));
    }
    let one_hop_per_line = msg.via().iter().count() == lines.count(&HeaderName::Via);
    if let Some(branch) = msg.via().first().branch().filter(|_| one_hop_per_line) {
        let other = format!(";branch={branch}x");
        add("repeat-branch", lines.edited(&HeaderName::Via, |l| appended(l, other.as_bytes())));
    }
    // Junk after a value the grammar ends.
    for name in [HeaderName::Via, HeaderName::CSeq, HeaderName::CallId, HeaderName::MaxForwards] {
        add("junk-after-value", lines.edited(&name, |l| appended(l, b" garbage")));
    }
    // A control byte is refused in every reading.
    add("vt-at-end", lines.edited(&HeaderName::From, |l| appended(l, b"\x0b")));
    add("ff-at-start", lines.edited(&HeaderName::To, |l| after_colon(l, b"\x0c")));
    // §7: the blank line ends the header block.
    if lines.body.is_empty() {
        add("no-blank-line", Some(lines.head()));
    }
    // §19.1.1: a URI parameter name appears once.
    let different = format!(";{REPEATED}=1;{REPEATED}=2");
    let mut repeated = vec![HeaderName::From];
    if one_contact(msg, lines) {
        repeated.push(HeaderName::Contact);
    }
    for name in repeated {
        add(
            "repeat-uri-parameter",
            lines.edited(&name, |l| with_uri_params(l, &different).unwrap_or_else(|| l.to_vec())),
        );
    }
    all.retain(|(_, raw)| raw != &lines.bytes());
    all
}

/// Each operator's message out of `msg`, which `parser` accepted, is refused by
/// `parser`.
pub fn assert_refused(msg: &SipMessage, parser: &CustomParser, reading: &str) {
    let Some(lines) = Lines::of(msg) else { return };
    if parser.parse(&lines.bytes()).is_err() {
        return;
    }
    for (operator, raw) in broken(msg, &lines) {
        assert!(
            parser.parse(&raw).is_err(),
            "{reading}: {operator} is accepted: {:?}",
            String::from_utf8_lossy(&raw)
        );
    }
}

/// A From or Contact URI of `msg` repeating [`REPEATED`] with an equal value is
/// accepted by `exempting`, whose set names that repeat for both, and refused by
/// `strict`, whose set is empty.
pub fn assert_exempted_repeat(msg: &SipMessage, exempting: &CustomParser, strict: &CustomParser) {
    let Some(lines) = Lines::of(msg) else { return };
    if exempting.parse(&lines.bytes()).is_err() {
        return;
    }
    let equal = format!(";{REPEATED}=1;{REPEATED}=1");
    let mut names = vec![HeaderName::From];
    if one_contact(msg, &lines) {
        names.push(HeaderName::Contact);
    }
    for name in names {
        let Some(at) = lines.first(&name) else { continue };
        if with_uri_params(&lines.headers[at], &equal).is_none() {
            continue;
        }
        let Some(raw) = lines.edited(&name, |l| with_uri_params(l, &equal).expect("checked"))
        else {
            continue;
        };
        let shown = String::from_utf8_lossy(&raw);
        if let Err(e) = exempting.parse(&raw) {
            panic!("an exempted {name} repeat is refused: {}: {shown:?}", e.reason);
        }
        assert!(strict.parse(&raw).is_err(), "a {name} repeat is accepted unnamed: {shown:?}");
    }
}

/// The mode over the lane's unmutated seeds, in the default lane: each seed an
/// operator breaks is refused, and the exempted repeat holds.
#[test]
fn every_seed_broken_by_an_operator_is_refused() {
    let (strict, exempting) = (CustomParser::new(), exempting());
    let mut checked = 0;
    for seed in super::seeds::messages() {
        if let Ok(msg) = strict.parse(&seed.bytes) {
            assert_refused(&msg, &strict, &seed.name);
            checked += 1;
        }
        if let Ok(msg) = exempting.parse(&seed.bytes) {
            assert_refused(&msg, &exempting, &seed.name);
            assert_exempted_repeat(&msg, &exempting, &strict);
        }
    }
    assert!(checked > 10, "{checked} seeds read");
}

/// A user part holding `;` and `?` (`user-unreserved`, RFC 3261 §25.1) keeps
/// them: the parameters go after the host, before the URI's headers.
#[test]
fn uri_parameters_follow_the_host() {
    let at = |line: &str| {
        let added = with_uri_params(line.as_bytes(), ";p=1").expect("a sip URI");
        String::from_utf8(added).expect("text")
    };
    assert_eq!(at("To: <sip:a;b?c@example.com>"), "To: <sip:a;b?c@example.com;p=1>");
    assert_eq!(at("To: <sip:example.com;x=y?h=v>"), "To: <sip:example.com;x=y;p=1?h=v>");
    assert_eq!(at("To: \"<q>\" <sips:a@example.com>"), "To: \"<q>\" <sips:a@example.com;p=1>");
}
