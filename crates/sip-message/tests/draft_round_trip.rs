//! ADR-0025 alignment pin: `thaw(&m).freeze()` is `m`.
//!
//! `thaw` and `freeze` are the only two functions that know both the message
//! shape and the draft shape, so this is the whole alignment surface. It is
//! driven over the parser's own torture corpus — RFC 4475, the IPv6 set, the
//! parameter-gap set and the CVE set — every fixture that parses at all.
//!
//! Identity is field identity, not byte identity: `freeze` writes the canonical
//! spelling of every header name (RFC 3261 §7.3.3 compact forms and mixed
//! casing collapse), and it restates Content-Length as the body actually
//! carried. Everything else — order, duplicates, values, body, and the typed
//! core — must come back unchanged.

use std::fs;
use std::path::{Path, PathBuf};

use sip_message::draft::{RequestDraft, ResponseDraft};
use sip_message::header::HeaderName;
use sip_message::types::SipHeader;
use sip_message::{CustomParser, SipMessage, SipParser};

const CATEGORIES: &[&str] =
    &["rfc4475-valid", "rfc4475-invalid", "strict-valid", "ipv6", "param-gaps", "cve"];

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.push("tests/fixtures");
    let mut out = Vec::new();
    for category in CATEGORIES {
        let dir = root.join(category);
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sip") {
                let name = format!("{category}/{}", file_stem(&path));
                out.push((name, fs::read(&path).expect("read fixture")));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn file_stem(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned()
}

/// The header list as `(canonical name, value)`, with Content-Length restated
/// as the body length the message actually carries.
fn lines(headers: &[SipHeader], body_len: usize) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|h| {
            let name = HeaderName::of(&h.name);
            let value = if name == HeaderName::ContentLength {
                body_len.to_string()
            } else {
                h.value.to_string()
            };
            (name.as_wire_str().to_owned(), value)
        })
        .collect()
}

#[test]
fn thaw_then_freeze_is_the_identity_on_the_torture_corpus() {
    let mut frozen_count = 0usize;
    let mut parsed_count = 0usize;
    let mut unfreezable: Vec<String> = Vec::new();

    for (name, raw) in fixtures() {
        let Ok(message) = CustomParser::new().parse(&raw) else { continue };
        parsed_count += 1;

        match message {
            SipMessage::Request(request) => {
                let frozen = match RequestDraft::thaw(&request).freeze() {
                    Ok(frozen) => frozen,
                    Err(e) => {
                        unfreezable.push(format!("{name}: {e}"));
                        continue;
                    }
                };
                assert_eq!(
                    lines(&frozen.headers, frozen.body.len()),
                    lines(&request.headers, request.body.len()),
                    "{name}: header list changed"
                );
                assert_eq!(frozen.body, request.body, "{name}: body changed");
                assert_eq!(frozen.method, request.method, "{name}: method changed");
                // RFC 3261 §16.6: a hop that is not retargeting forwards the
                // Request-URI it received, octet for octet.
                assert_eq!(frozen.uri, request.uri, "{name}: Request-URI changed");
                assert_eq!(frozen.version, request.version, "{name}: version changed");
                assert_eq!(frozen.call_id, request.call_id, "{name}: Call-ID changed");
                assert_eq!(frozen.cseq, request.cseq, "{name}: CSeq changed");
                assert_eq!(frozen.from.tag, request.from.tag, "{name}: From-tag changed");
                assert_eq!(frozen.to.tag, request.to.tag, "{name}: To-tag changed");
                assert_eq!(frozen.via.len(), request.via.len(), "{name}: Via count changed");

                // The frozen message carries a real image: parsing its own
                // bytes yields the same header list and body.
                let reparsed = CustomParser::new()
                    .parse(&frozen.raw)
                    .unwrap_or_else(|e| panic!("{name}: frozen bytes do not parse: {}", e.reason));
                assert_eq!(reparsed.headers(), frozen.headers, "{name}: image disagrees");

                // Freezing is idempotent: a second pass is byte-identical.
                let again = frozen.thaw().freeze().expect("a frozen message thaws complete");
                assert_eq!(again.raw, frozen.raw, "{name}: second freeze differs");
                frozen_count += 1;
            }
            SipMessage::Response(response) => {
                let frozen = match ResponseDraft::thaw(&response).freeze() {
                    Ok(frozen) => frozen,
                    Err(e) => {
                        unfreezable.push(format!("{name}: {e}"));
                        continue;
                    }
                };
                assert_eq!(
                    lines(&frozen.headers, frozen.body.len()),
                    lines(&response.headers, response.body.len()),
                    "{name}: header list changed"
                );
                assert_eq!(frozen.body, response.body, "{name}: body changed");
                assert_eq!(frozen.status, response.status, "{name}: status changed");
                assert_eq!(frozen.reason, response.reason, "{name}: reason changed");
                assert_eq!(frozen.version, response.version, "{name}: version changed");
                assert_eq!(frozen.call_id, response.call_id, "{name}: Call-ID changed");
                assert_eq!(frozen.cseq, response.cseq, "{name}: CSeq changed");

                let reparsed = CustomParser::new()
                    .parse(&frozen.raw)
                    .unwrap_or_else(|e| panic!("{name}: frozen bytes do not parse: {}", e.reason));
                assert_eq!(reparsed.headers(), frozen.headers, "{name}: image disagrees");

                let again = frozen.thaw().freeze().expect("a frozen message thaws complete");
                assert_eq!(again.raw, frozen.raw, "{name}: second freeze differs");
                frozen_count += 1;
            }
        }
    }

    assert!(parsed_count >= 20, "only {parsed_count} fixtures parsed — the pin would be vacuous");
    assert!(unfreezable.is_empty(), "a parsed message failed to freeze: {unfreezable:#?}");
    assert_eq!(frozen_count, parsed_count, "every parsed fixture must survive a thaw/freeze");
}
