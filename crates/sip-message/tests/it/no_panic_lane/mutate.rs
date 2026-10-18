//! Structured mutations of a message or a description: truncation at an
//! offset, a header line duplicated or spliced in, a protocol number replaced
//! by an extreme value, a delimiter or a multi-byte character inserted. Every
//! index is taken modulo what the input holds, so any value applies to any seed.

use proptest::prelude::*;

/// Header lines that carry protocol numbers, with the template of the value an
/// extreme number is written into (`{}` is the number).
pub const NUMERIC_HEADERS: &[(&str, &str)] = &[
    ("Content-Length", "{}"),
    ("l", "{}"),
    ("CSeq", "{} INVITE"),
    ("Expires", "{}"),
    ("Session-Expires", "{};refresher=uac"),
    ("x", "{}"),
    ("Min-SE", "{}"),
    ("RSeq", "{}"),
    ("RAck", "{} {} INVITE"),
    ("Max-Forwards", "{}"),
    ("Retry-After", "{};duration={}"),
    ("Timestamp", "{} {}"),
    ("X-Incarnation", "{}"),
    ("Via", "SIP/2.0/UDP 192.0.2.1:{};branch=z9hG4bK-x;rport={}"),
    ("Contact", "<sip:a@192.0.2.1:{}>;expires={};q={}"),
];

/// Extreme numbers: the type bounds either side, signs, exponents, empties.
pub const EXTREMES: &[&str] = &[
    "0",
    "-1",
    "+1",
    "65535",
    "65536",
    "2147483647",
    "2147483648",
    "4294967295",
    "4294967296",
    "9223372036854775807",
    "18446744073709551615",
    "18446744073709551616",
    "340282366920938463463374607431768211456",
    "1e9",
    "0x10",
    "",
    " ",
    "1.5",
    "é",
];

/// The [`EXTREMES`] every digit run of a seed is replaced by in the corpus:
/// past `u32`, past `u64`, a sign, nothing.
const RUN_EXTREMES: &[usize] = &[7, 8, 11, 1, 15];

/// Bytes the grammar gives meaning to, and characters wider than one byte.
pub const TOKENS: &[&str] = &[
    "\r\n",
    "\r",
    "\n",
    "\r\n\r\n",
    " ",
    "\t",
    ":",
    ";",
    ",",
    "=",
    "<",
    ">",
    "\"",
    "\\",
    "@",
    "[",
    "]",
    "?",
    "&",
    "%",
    "%0",
    "/",
    "*",
    "\0",
    "\u{7f}",
    "é",
    "€",
    "𝄞",
    "\u{feff}",
    "\r\n ",
    "\r\n\t",
    "--",
    "m=",
    "a=",
    "c=",
    "o=",
    "\r\nm=audio 0 RTP/AVP 0\r\n",
];

/// Header lines spliced into a message, valid and not.
pub const SPLICES: &[&str] = &[
    "Content-Length: 99999",
    "Content-Length: 0",
    "Content-Type: multipart/mixed;boundary=",
    "Content-Type: multipart/mixed;boundary=\"\"",
    "Content-Type: multipart/mixed;boundary=--",
    "Content-Type: application/sdp",
    "Content-Type: message/sipfrag",
    "Content-Type: application/mediaservercontrol+xml",
    "Via: SIP/2.0/UDP [2001:db8::1",
    "Via: SIP/2.0/UDP 192.0.2.1:99999;branch=",
    "Via: SIP/2.0/",
    "Via: ,,,",
    "From: <sip:a@",
    "From: \"unterminated <sip:a@b>",
    "To: <sip:b@example.com>;tag=",
    "To: sip:b@example.com;tag=x;tag=y",
    "Call-ID: ",
    "CSeq: 1",
    "CSeq: INVITE",
    "CSeq: 4294967296 INVITE",
    "Contact: *",
    "Contact: <sip:a@[::1]:>;expires=",
    "Route: <sip:a;lr>,",
    "Record-Route: <",
    "RAck: 1",
    "RSeq: -1",
    "Session-Expires: ;refresher=",
    "Refer-To: <sip:a@b?Replaces=%",
    "Reason: Q.850;cause=",
    "Retry-After: (",
    "Authorization: Digest realm=\"",
    "P-Charging-Vector: icid-value",
    "X-Trace-Sample: é",
    "X-Readiness: new",
    "Resource-Priority: .",
    "Event: ;id=",
    "é: é",
    ": empty name",
    "NoColon",
];

/// One mutation, its indices free: [`apply`] reduces them against the input.
#[derive(Clone, Debug)]
pub enum Mutation {
    Truncate(usize),
    DuplicateHeader(usize),
    Splice { at: usize, line: usize },
    Extreme { header: usize, value: usize },
    Insert { at: usize, token: usize },
    DropByte(usize),
    BodyLine { line: usize, token: usize },
    Number { nth: usize, value: usize },
}

/// The byte ranges of the header lines (start line excluded), CRLF or LF
/// ended, up to the blank line.
fn header_lines(raw: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < raw.len() {
        let end =
            raw[start..].iter().position(|&b| b == b'\n').map_or(raw.len(), |p| start + p + 1);
        let content = &raw[start..end];
        let blank = content == b"\r\n" || content == b"\n";
        if blank {
            break;
        }
        if !first {
            lines.push(start..end);
        }
        first = false;
        start = end;
    }
    lines
}

/// Where the body starts: after the first blank line, else the end.
fn body_start(raw: &[u8]) -> usize {
    let crlf = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    let lf = raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b).unwrap_or(raw.len()),
    }
}

fn header_start_offset(raw: &[u8]) -> usize {
    raw.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| p + 1)
}

fn extreme_value(template: &str, value: &str) -> String {
    template.replace("{}", value)
}

/// `raw` with `m` applied.
pub fn apply(raw: &[u8], m: &Mutation) -> Vec<u8> {
    let mut out = raw.to_vec();
    match *m {
        Mutation::Truncate(at) => out.truncate(at % (raw.len() + 1)),
        Mutation::DuplicateHeader(i) => {
            let lines = header_lines(raw);
            if !lines.is_empty() {
                let line = lines[i % lines.len()].clone();
                let copy = raw[line.clone()].to_vec();
                out.splice(line.end..line.end, copy);
            }
        }
        Mutation::Splice { at, line } => {
            let lines = header_lines(raw);
            let offset = if lines.is_empty() {
                header_start_offset(raw)
            } else {
                let i = at % (lines.len() + 1);
                lines.get(i).map_or(lines[lines.len() - 1].end, |r| r.start)
            };
            let text = format!("{}\r\n", SPLICES[line % SPLICES.len()]);
            out.splice(offset..offset, text.into_bytes());
        }
        Mutation::Extreme { header, value } => {
            let (name, template) = NUMERIC_HEADERS[header % NUMERIC_HEADERS.len()];
            let number = EXTREMES[value % EXTREMES.len()];
            let lines = header_lines(raw);
            let target = lines.iter().find(|r| {
                let text = &raw[(*r).clone()];
                text.len() > name.len()
                    && text[..name.len()].eq_ignore_ascii_case(name.as_bytes())
                    && matches!(text[name.len()], b':' | b' ' | b'\t')
            });
            let replacement =
                format!("{name}: {}\r\n", extreme_value(template, number)).into_bytes();
            match target {
                Some(r) => {
                    out.splice(r.clone(), replacement);
                }
                None => {
                    let offset = header_start_offset(raw);
                    out.splice(offset..offset, replacement);
                }
            }
        }
        Mutation::Insert { at, token } => {
            let at = at % (raw.len() + 1);
            out.splice(at..at, TOKENS[token % TOKENS.len()].bytes());
        }
        Mutation::DropByte(at) => {
            if !raw.is_empty() {
                out.remove(at % raw.len());
            }
        }
        Mutation::BodyLine { line, token } => {
            let body = body_start(raw);
            let starts: Vec<usize> = std::iter::once(body)
                .chain(
                    raw[body..]
                        .iter()
                        .enumerate()
                        .filter(|(_, &b)| b == b'\n')
                        .map(|(i, _)| body + i + 1),
                )
                .filter(|&s| s <= raw.len())
                .collect();
            let at = starts[line % starts.len()];
            out.splice(at..at, TOKENS[token % TOKENS.len()].bytes());
        }
        Mutation::Number { nth, value } => {
            let runs = digit_runs(raw);
            if !runs.is_empty() {
                let run = runs[nth % runs.len()].clone();
                out.splice(run, EXTREMES[value % EXTREMES.len()].bytes());
            }
        }
    }
    out
}

/// The maximal runs of ASCII digits, in order.
fn digit_runs(raw: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i].is_ascii_digit() {
            let start = i;
            while i < raw.len() && raw[i].is_ascii_digit() {
                i += 1;
            }
            runs.push(start..i);
        } else {
            i += 1;
        }
    }
    runs
}

/// One of the mutations, any indices.
pub fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        3 => any::<usize>().prop_map(Mutation::Truncate),
        2 => any::<usize>().prop_map(Mutation::DuplicateHeader),
        3 => (any::<usize>(), any::<usize>()).prop_map(|(at, line)| Mutation::Splice { at, line }),
        3 => (any::<usize>(), any::<usize>())
            .prop_map(|(header, value)| Mutation::Extreme { header, value }),
        4 => (any::<usize>(), any::<usize>()).prop_map(|(at, token)| Mutation::Insert { at, token }),
        2 => any::<usize>().prop_map(Mutation::DropByte),
        3 => (any::<usize>(), any::<usize>())
            .prop_map(|(line, token)| Mutation::BodyLine { line, token }),
        4 => (any::<usize>(), any::<usize>()).prop_map(|(nth, value)| Mutation::Number { nth, value }),
    ]
}

/// The deterministic families of the equivalence corpus, each a list of
/// mutations of one seed: every truncation offset, every header duplicated,
/// every splice at the first header, every extreme number in every numeric
/// header and in place of every digit run, every token at every line start of
/// the body.
pub fn families(raw: &[u8]) -> Vec<(&'static str, Vec<Mutation>)> {
    let headers = header_lines(raw).len();
    let body_lines = raw[body_start(raw)..].iter().filter(|&&b| b == b'\n').count() + 1;
    let runs = digit_runs(raw).len();
    vec![
        ("truncate", (0..=raw.len()).map(Mutation::Truncate).collect()),
        ("duplicate", (0..headers).map(Mutation::DuplicateHeader).collect()),
        ("splice", (0..SPLICES.len()).map(|line| Mutation::Splice { at: 0, line }).collect()),
        (
            "extreme",
            (0..NUMERIC_HEADERS.len())
                .flat_map(|header| {
                    (0..EXTREMES.len()).map(move |value| Mutation::Extreme { header, value })
                })
                .collect(),
        ),
        (
            "number",
            (0..runs)
                .flat_map(|nth| {
                    RUN_EXTREMES.iter().map(move |&value| Mutation::Number { nth, value })
                })
                .collect(),
        ),
        (
            "body",
            (0..body_lines)
                .flat_map(|line| {
                    (0..TOKENS.len()).map(move |token| Mutation::BodyLine { line, token })
                })
                .collect(),
        ),
    ]
}
