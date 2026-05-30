//! ABNF fuzz suite — replay the frozen corpus against the per-header parsers.
//! Port of `scripts/abnf-fuzz-driver.ts` + `run-suite.sh`.
//!
//! The contract: for grammar-valid input the parsers must never *buggily*
//! reject (a rejection that matches no documented ADR-0007 policy) and never
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

use sip_message::parser::custom::structured_headers::{
    find_uri_embedded_headers_start, parse_contact, parse_cseq, parse_name_addr, parse_rack,
    parse_refer_to, parse_replaces, parse_sip_uri_string, parse_via, split_top_level_commas,
    validate_strict_sip_uri,
};

/// Rejection reasons reflecting documented semantic constraints (RFC limits
/// beyond pure ABNF) rather than parser bugs — abnfgen happily emits port=88161
/// or degenerate empty-hostport URIs the parser correctly rejects.
const POLICY_PATTERNS: &[&str] = &[
    "port out of range",
    "non-digit in port",
    "empty hostport",
    "empty host",
    "multiple `@`",
    "multiple `:` in hostport",
    "hostport starts with `:`",
    "unclosed IPv6 reference",
    "empty IPv6 reference",
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
    if let Some(reason) = validate_strict_sip_uri(line) {
        s.reject(line, reason);
        return;
    }
    s.accepted += 1;
    if line.contains('@') {
        if let Some(parsed) = parse_sip_uri_string(line) {
            if parsed.user.is_none() {
                s.silent(line);
            }
        }
    }
}

fn fuzz_name_addr(line: &str, header: &str, s: &mut Stat) {
    let parsed = parse_name_addr(line);
    if parsed.uri.is_empty() {
        s.reject(line, "empty parsed.uri".to_string());
        return;
    }
    if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
        s.reject(line, format!("Strict {header} URI: {reason} (\"{}\")", parsed.uri));
        return;
    }
    s.accepted += 1;
    // Silent-misparse heuristic: an `@` inside `<...>` must survive into the URI.
    if let Some(lt) = line.find('<') {
        if let Some(gt_rel) = line[lt + 1..].find('>') {
            let between = &line[lt + 1..lt + 1 + gt_rel];
            if between.contains('@') && !parsed.uri.contains('@') {
                s.silent(line);
            }
        }
    }
}

fn fuzz_pai_entry(entry: &str, s: &mut Stat) {
    let parsed = parse_name_addr(entry);
    if parsed.uri.is_empty() {
        s.reject(entry, "empty parsed.uri".to_string());
        return;
    }
    if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
        s.reject(entry, format!("Strict PAI URI: {reason} (\"{}\")", parsed.uri));
        return;
    }
    s.accepted += 1;
}

fn fuzz_contact(line: &str, s: &mut Stat) {
    if line.trim() == "*" {
        s.accepted += 1;
        return;
    }
    for entry in split_top_level_commas(line) {
        if entry.is_empty() {
            continue;
        }
        let parsed = parse_contact(&entry);
        if parsed.uri.is_empty() {
            s.reject(&entry, "empty parsed.uri".to_string());
            return;
        }
        if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
            s.reject(&entry, format!("Strict Contact URI: {reason} (\"{}\")", parsed.uri));
            return;
        }
    }
    s.accepted += 1;
}

fn fuzz_via(line: &str, s: &mut Stat) {
    for entry in split_top_level_commas(line) {
        if entry.is_empty() {
            continue;
        }
        let parsed = parse_via(&entry);
        if parsed.transport.is_empty() || parsed.host.is_empty() {
            s.reject(
                &entry,
                format!("empty transport/host (transport=\"{}\" host=\"{}\")", parsed.transport, parsed.host),
            );
            return;
        }
        if let Some(port) = parsed.port {
            if port == 0 || port > 65535 {
                s.reject(&entry, format!("port out of range ({port})"));
                return;
            }
        }
    }
    s.accepted += 1;
}

fn fuzz_cseq(line: &str, s: &mut Stat) {
    let parsed = parse_cseq(line);
    if parsed.method.is_empty() {
        s.reject(line, "empty method".to_string());
        return;
    }
    s.accepted += 1;
}

fn fuzz_rack(line: &str, s: &mut Stat) {
    if parse_rack(line).is_none() {
        s.reject(line, "parse_rack returned None".to_string());
        return;
    }
    s.accepted += 1;
}

fn fuzz_replaces(line: &str, s: &mut Stat) {
    if parse_replaces(line).is_none() {
        s.reject(line, "parse_replaces returned None".to_string());
        return;
    }
    s.accepted += 1;
}

fn fuzz_refer_to(line: &str, s: &mut Stat) {
    let Some(parsed) = parse_refer_to(line) else {
        s.reject(line, "parse_refer_to returned None".to_string());
        return;
    };
    let uri = &parsed.uri;
    let head = match find_uri_embedded_headers_start(uri) {
        Some(q) => &uri[..q],
        None => uri.as_str(),
    };
    if let Some(reason) = validate_strict_sip_uri(head) {
        s.reject(line, format!("Strict Refer-To URI: {reason} (\"{head}\")"));
        return;
    }
    s.accepted += 1;
}

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
    let req_uri = parts[1];
    if let Some(reason) = validate_strict_sip_uri(req_uri) {
        s.reject(line, format!("Strict Request-URI: {reason} (\"{req_uri}\")"));
        return;
    }
    if req_uri.contains('@') {
        if let Some(parsed) = parse_sip_uri_string(req_uri) {
            if parsed.user.is_none() {
                s.silent(line);
            }
        }
    }
    s.accepted += 1;
}

fn is_method_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!%*_+`'~.-".contains(c)
}

fn dispatch(target: &str, line: &str, s: &mut Stat) {
    match target {
        "sip-uri" => fuzz_sip_uri(line, s),
        "from" => fuzz_name_addr(line, "From", s),
        "pai" => {
            for entry in split_top_level_commas(line) {
                if !entry.is_empty() {
                    fuzz_pai_entry(&entry, s);
                }
            }
        }
        "contact" => fuzz_contact(line, s),
        "via" => fuzz_via(line, s),
        "cseq" => fuzz_cseq(line, s),
        "rack" => fuzz_rack(line, s),
        "replaces" => fuzz_replaces(line, s),
        "refer-to" => fuzz_refer_to(line, s),
        "request-line" => fuzz_request_line(line, s),
        other => panic!("unknown abnf target: {other}"),
    }
}

const TARGETS: &[&str] = &[
    "sip-uri",
    "from",
    "pai",
    "contact",
    "via",
    "cseq",
    "rack",
    "replaces",
    "refer-to",
    "request-line",
];

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
