//! The frozen ABNF corpus through the production path. Each line of the vendored
//! grammars' corpus (`tests/abnf/corpus/<target>.txt`) is wrapped in a whole
//! message, read by the wire parser, then by its typed reader. A refusal maps to
//! a stated rule or fails a conformance row (`conformance::stated`); a value that
//! reads but drops an obvious field fails `frozen/silent-misparse`.
//!
//! The corpus is committed and frozen: `cargo run -p xtask -- abnf-regen [N]`
//! regenerates it (needs `abnfgen`). A target with no corpus file fails the test.

use std::fs;
use std::path::PathBuf;

use sip_message::header;
use sip_message::SipMessage;

use super::conformance::message::{read, typed, Template};
use super::conformance::ratchet::{settle_rows, Failures};
use super::conformance::stated::failing_row;

const TARGETS: &[&str] =
    &["sip-uri", "from", "pai", "contact", "via", "cseq", "rack", "refer-to", "request-line"];

/// The whole message `line` of `target` is read in.
fn wrap(target: &str, line: &str) -> Vec<u8> {
    let options = Template::request("OPTIONS");
    match target {
        "sip-uri" => options.with("to", format!("To: <{line}>")),
        "from" => options.with("from", format!("From: {line}")),
        "pai" => options.with("pai", format!("P-Asserted-Identity: {line}")),
        "contact" => Template::request("REGISTER").with("contact", format!("Contact: {line}")),
        "via" => options.with("lower-via", format!("Via: {line}")),
        "cseq" => {
            let method = line.split_ascii_whitespace().last().unwrap_or_default();
            Template::request(method).with("cseq", format!("CSeq: {line}"))
        }
        "rack" => Template::request("PRACK").with("rack", format!("RAck: {line}")),
        "refer-to" => Template::request("REFER").with("refer-to", format!("Refer-To: {line}")),
        "request-line" => {
            let method = line.split(' ').next().unwrap_or_default();
            options.with("cseq", format!("CSeq: 1 {method}")).start_line(line)
        }
        other => panic!("unknown abnf target: {other}"),
    }
    .bytes()
}

fn typed_read(target: &str, msg: &SipMessage) -> Result<(), String> {
    match target {
        "sip-uri" => typed::<header::To>(msg),
        "from" => typed::<header::From>(msg),
        "pai" => typed::<header::PAssertedIdentity>(msg),
        "contact" => typed::<header::Contact>(msg),
        "via" => typed::<header::Via>(msg),
        "cseq" => typed::<header::CSeq>(msg),
        "rack" => typed::<header::RAck>(msg),
        "refer-to" => typed::<header::ReferTo>(msg),
        _ => Ok(()),
    }
}

/// An `@` inside the URI of `line` that the reading lost.
fn lost_user(target: &str, line: &str, msg: &SipMessage) -> bool {
    match target {
        "sip-uri" => line.contains('@') && msg.to().uri().user().is_none(),
        "from" => {
            let inner = line.split_once('<').and_then(|(_, rest)| rest.split_once('>'));
            let written = inner.is_some_and(|(between, _)| between.contains('@'));
            written && !msg.from().uri().text().contains('@')
        }
        "request-line" => {
            let uri = line.split(' ').nth(1).unwrap_or_default();
            let user = |m: &SipMessage| match m {
                SipMessage::Request(r) => r.request_uri().user().is_none(),
                SipMessage::Response(_) => true,
            };
            uri.contains('@') && user(msg)
        }
        _ => false,
    }
}

fn corpus(target: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/abnf/corpus")
        .join(format!("{target}.txt"));
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("corpus of {target}: {e}"))
}

#[test]
fn the_frozen_corpus_reads_through_the_production_path() {
    let mut failures = Failures::default();
    for &target in TARGETS {
        let text = corpus(target);
        let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
        assert!(!lines.is_empty(), "{target}: an empty corpus proves nothing");
        let mut accepted = 0;
        for line in &lines {
            let outcome = match read(&wrap(target, line)) {
                Ok(msg) if lost_user(target, line, &msg) => Err("silent-misparse".to_string()),
                Ok(msg) => typed_read(target, &msg),
                Err(e) => Err(e.reason),
            };
            match outcome {
                Ok(()) => accepted += 1,
                Err(reason) if reason == "silent-misparse" => {
                    failures.note("silent-misparse", target, line, &reason);
                }
                Err(reason) => {
                    if let Some(row) = failing_row(&reason) {
                        failures.note(row, target, line, &reason);
                    }
                }
            }
        }
        eprintln!("frozen[{target:<13}] total={:<5} accepted={accepted}", lines.len());
    }
    settle_rows("frozen", &failures);
}
