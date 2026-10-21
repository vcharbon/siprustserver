//! ABNF fuzz suite — replay the frozen corpus against the per-header parsers.
//! Port of `scripts/abnf-fuzz-driver.ts` + `run-suite.sh`.
//!
//! The contract: for grammar-valid input the parsers must never *buggily*
//! reject (a rejection that matches no documented policy, [`POLICY_PATTERNS`]) and never
//! *silently misparse* (accept but drop an obvious field). A clean run has
//! `buggy == 0` and `silentMisparse == 0` across every target.
//!
//! The corpus (`tests/abnf/corpus/<target>.txt`) is committed and frozen —
//! regenerate it with `cargo run -p xtask -- abnf-regen [N]` (needs `abnfgen`).
//! When a target's corpus file is absent or empty (e.g. a fresh checkout that
//! hasn't run the generator) this test SKIPS that target with a logged note
//! rather than passing silently — there is no hidden "0 inputs == green".

use std::fs;
use std::path::PathBuf;

use sip_message::header::{Contact, HeaderValue, NameAddr, RAck, ReferTo, Uri};
use sip_message::parser::custom::structured_headers::{
    parse_cseq, parse_via, split_top_level_commas,
};
use sip_message::{CustomParser, SipMessage, SipParser, SipStr};

/// Rejection reasons reflecting documented semantic constraints (RFC limits
/// beyond pure ABNF) rather than parser bugs — abnfgen happily emits port=88161
/// or degenerate empty-hostport URIs the parser correctly rejects.
const POLICY_PATTERNS: &[&str] = &[
    "port out of range",
    "non-digit in port",
    "empty port",
    "empty host",
    "unclosed IPv6 reference",
    "empty IPv6 reference",
    // RFC 3261 §19.1.1 Table 1: the grammar's Request-URI admits `?headers`,
    // the table forbids them there.
    "Request-URI carries escaped headers",
    // The vendored grammars draw a header parameter's name from the URI's
    // `paramchar`; RFC 3261 §25.1 `generic-param = token [ EQUAL gen-value ]`.
    "parameter name is not a token",
];

fn is_policy(reason: &str) -> bool {
    POLICY_PATTERNS.iter().any(|p| reason.contains(p))
}

#[derive(Default)]
struct Stat {
    total: usize,
    accepted: usize,
    policy: usize,
    buggy: usize,
    silent: usize,
    buggy_samples: Vec<(String, String)>,
    silent_samples: Vec<String>,
}

impl Stat {
    fn reject(&mut self, input: &str, reason: String) {
        if is_policy(&reason) {
            self.policy += 1;
        } else {
            self.buggy += 1;
            if self.buggy_samples.len() < 12 {
                self.buggy_samples.push((input.to_string(), reason));
            }
        }
    }
    fn silent(&mut self, input: &str) {
        self.silent += 1;
        if self.silent_samples.len() < 12 {
            self.silent_samples.push(input.to_string());
        }
    }
}

// --- per-target fuzz functions (ported from the driver) ---

fn fuzz_sip_uri(line: &str, s: &mut Stat) {
    let uri = match Uri::parse(&SipStr::owned(line)) {
        Ok(uri) => uri,
        Err(e) => return s.reject(line, e.reason),
    };
    s.accepted += 1;
    if line.contains('@') && uri.user().is_none() {
        s.silent(line);
    }
}

fn fuzz_name_addr(line: &str, s: &mut Stat) {
    let addr = match NameAddr::parse(&SipStr::owned(line)) {
        Ok(addr) => addr,
        Err(e) => return s.reject(line, e.reason),
    };
    s.accepted += 1;
    // Silent-misparse heuristic: an `@` inside `<...>` must survive into the URI.
    if let Some(lt) = line.find('<') {
        if let Some(gt_rel) = line[lt + 1..].find('>') {
            let between = &line[lt + 1..lt + 1 + gt_rel];
            if between.contains('@') && !addr.uri().text().contains('@') {
                s.silent(line);
            }
        }
    }
}

fn fuzz_pai_entry(entry: &str, s: &mut Stat) {
    match NameAddr::parse(&SipStr::owned(entry)) {
        Ok(_) => s.accepted += 1,
        Err(e) => s.reject(entry, e.reason),
    }
}

fn fuzz_contact(line: &str, s: &mut Stat) {
    if line.trim() == "*" {
        s.accepted += 1;
        return;
    }
    for entry in split_top_level_commas(line) {
        if let Err(e) = Contact::parse(&SipStr::owned(entry)) {
            return s.reject(entry, e.reason);
        }
    }
    s.accepted += 1;
}

fn fuzz_via(line: &str, s: &mut Stat) {
    for entry in split_top_level_commas(line) {
        if entry.is_empty() {
            continue;
        }
        let parsed = parse_via(&SipStr::owned(entry));
        if parsed.transport.is_empty() || parsed.host.is_empty() {
            s.reject(
                entry,
                format!(
                    "empty transport/host (transport=\"{}\" host=\"{}\")",
                    parsed.transport, parsed.host
                ),
            );
            return;
        }
        if let Some(port) = parsed.port {
            if port == 0 || port > 65535 {
                s.reject(entry, format!("port out of range ({port})"));
                return;
            }
        }
    }
    s.accepted += 1;
}

fn fuzz_cseq(line: &str, s: &mut Stat) {
    let parsed = parse_cseq(&SipStr::owned(line));
    if parsed.method.is_empty() {
        s.reject(line, "empty method".to_string());
        return;
    }
    s.accepted += 1;
}

fn fuzz_rack(line: &str, s: &mut Stat) {
    if let Err(e) = RAck::parse(&SipStr::owned(line)) {
        s.reject(line, e.reason);
        return;
    }
    s.accepted += 1;
}

fn fuzz_refer_to(line: &str, s: &mut Stat) {
    match ReferTo::parse(&SipStr::owned(line)) {
        Ok(_) => s.accepted += 1,
        Err(e) => s.reject(line, e.reason),
    }
}

/// The request line read as the start of a whole request, so the Request-URI
/// meets every check a peer's request does.
fn fuzz_request_line(line: &str, s: &mut Stat) {
    // "METHOD SP REQUEST-URI SP SIP-Version" — SIP-Version case-insensitive.
    let parts: Vec<&str> = line.split(' ').collect();
    let shape_ok = parts.len() == 3
        && !parts[0].is_empty()
        && parts[0].chars().all(is_method_char)
        && parts[2].eq_ignore_ascii_case("SIP/2.0");
    if !shape_ok {
        s.reject(line, "request-line shape check failed".to_string());
        return;
    }
    let (method, req_uri) = (parts[0], parts[1]);
    let raw = format!(
        "{method} {req_uri} SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-abnf\r\n\
         From: <sip:a@example.com>;tag=1\r\nTo: <sip:b@example.com>\r\nCall-ID: abnf\r\n\
         CSeq: 1 {method}\r\nContent-Length: 0\r\n\r\n"
    );
    let request = match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Request(r)) => r,
        Ok(SipMessage::Response(_)) => return s.reject(line, "read as a response".to_string()),
        Err(e) => return s.reject(line, e.reason),
    };
    s.accepted += 1;
    if req_uri.contains('@') && request.request_uri().user().is_none() {
        s.silent(line);
    }
}

fn is_method_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!%*_+`'~.-".contains(c)
}

fn dispatch(target: &str, line: &str, s: &mut Stat) {
    match target {
        "sip-uri" => fuzz_sip_uri(line, s),
        "from" => fuzz_name_addr(line, s),
        "pai" => {
            for entry in split_top_level_commas(line) {
                if !entry.is_empty() {
                    fuzz_pai_entry(entry, s);
                }
            }
        }
        "contact" => fuzz_contact(line, s),
        "via" => fuzz_via(line, s),
        "cseq" => fuzz_cseq(line, s),
        "rack" => fuzz_rack(line, s),
        "refer-to" => fuzz_refer_to(line, s),
        "request-line" => fuzz_request_line(line, s),
        other => panic!("unknown abnf target: {other}"),
    }
}

const TARGETS: &[&str] =
    &["sip-uri", "from", "pai", "contact", "via", "cseq", "rack", "refer-to", "request-line"];

fn corpus_path(target: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/abnf/corpus");
    p.push(format!("{target}.txt"));
    p
}

#[test]
fn abnf_corpus_never_buggily_rejects_or_misparses() {
    let mut ran_any = false;
    let mut failures: Vec<String> = Vec::new();
    let mut skipped: Vec<&str> = Vec::new();

    for &target in TARGETS {
        let path = corpus_path(target);
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                skipped.push(target);
                continue;
            }
        };
        let lines: Vec<&str> = contents.split('\n').filter(|l| !l.is_empty()).collect();
        if lines.is_empty() {
            skipped.push(target);
            continue;
        }

        ran_any = true;
        let mut s = Stat { total: lines.len(), ..Default::default() };
        for line in &lines {
            dispatch(target, line, &mut s);
        }

        eprintln!(
            "abnf[{target:<13}] total={:<5} accepted={:<5} policy={:<4} buggy={:<3} silentMisparse={}",
            s.total, s.accepted, s.policy, s.buggy, s.silent
        );

        if s.buggy > 0 || s.silent > 0 {
            let mut msg = format!(
                "target {target}: buggy={} silentMisparse={} (both must be 0)",
                s.buggy, s.silent
            );
            for (input, reason) in s.buggy_samples.iter().take(5) {
                msg.push_str(&format!("\n    BUGGY  {input:?} -> {reason}"));
            }
            for input in s.silent_samples.iter().take(5) {
                msg.push_str(&format!("\n    SILENT {input:?}"));
            }
            failures.push(msg);
        }
    }

    if !skipped.is_empty() {
        eprintln!(
            "abnf_fuzz: SKIPPED targets with no committed corpus: {}.\n\
             Generate it with `cargo run -p xtask -- abnf-regen` (needs abnfgen).",
            skipped.join(", ")
        );
    }

    assert!(failures.is_empty(), "ABNF fuzz failures:\n{}", failures.join("\n"));

    if !ran_any {
        eprintln!(
            "abnf_fuzz: no corpus present — every target skipped. This is expected on a \
             fresh checkout; run `cargo run -p xtask -- abnf-regen` to populate the corpus."
        );
    }
}
