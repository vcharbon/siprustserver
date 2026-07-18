//! Tiered expectation matching — assert an INBOUND parsed message against a
//! [`MessageTemplate`], returning a structured [`Mismatch`] on the first
//! divergence.
//!
//! The tier model (mirrors template emission, [`crate::template`]):
//!
//! - **tier-1 / structural** (`Via`, `From`, `To`, `Call-ID`, `CSeq`,
//!   `Max-Forwards`, `Content-Length`): the live dialog owns these, so they are
//!   NOT value-compared — only presence is checked (syntactic validity is
//!   already guaranteed: the inbound was parsed).
//! - **remote-target** (`Contact`, `Route`, `Record-Route`): compared by their
//!   PARAMETERS only, modulo [`MatchOpts::ignore_params`] — the URI base is
//!   regenerated / topology-dependent (deferred tier-2 role-normalization), so
//!   benign environment-added params (an LB's `primary`/`backup` ids) do not
//!   fail certification while a meaningful param drift does.
//! - **frozen** (everything else, incl. `Content-Type` and every extension
//!   header): value-compared byte-wise against the template's parsed value.
//!
//! Header NAME comparison is canonical (a captured compact `k:` matches an
//! inbound `Supported:` and vice versa); duplicate-header rows must match in
//! count and per-row order; a template body is compared byte-wise.
//!
//! The match is TEMPLATE-DRIVEN: it asserts each template header is satisfied by
//! the inbound; inbound headers the template does not mention (a stack-added
//! `Allow`/`Supported`/`Date`) are not flagged.

use std::collections::BTreeMap;

use crate::message_helpers::parse_sip_uri;
use crate::parser::custom::compact_forms::expand_compact_form;
use crate::template::{MessageTemplate, TemplateStart};
use crate::types::SipMessage;

/// Tier-1 headers the live dialog owns — presence-checked, never value-compared.
const TIER1: &[&str] =
    &["via", "from", "to", "call-id", "cseq", "max-forwards", "content-length"];

/// Remote-target headers — compared by parameters only, modulo `ignore_params`.
const REMOTE_TARGET: &[&str] = &["contact", "route", "record-route"];

/// Options controlling [`MessageTemplate::match_inbound`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchOpts {
    /// Parameter NAMES excluded from comparison on remote-target headers
    /// (`Contact`/`Route`/`Record-Route`), on BOTH sides — benign
    /// environment-added params (LB ids, etc.). Case-insensitive. Params not
    /// listed compare exactly.
    pub ignore_params: Vec<String>,
}

impl MatchOpts {
    /// Ignore these parameter names on remote-target headers.
    pub fn ignoring<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        MatchOpts { ignore_params: names.into_iter().map(Into::into).collect() }
    }
}

/// The first divergence [`MessageTemplate::match_inbound`] found — structured
/// for assertions and readable via [`Display`](std::fmt::Display) in a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// The message kind or request-method / response-status differs.
    StartLine { expected: String, got: String },
    /// A tier-1 header the template carries is absent from the inbound.
    MissingHeader { name: String },
    /// A compared header appears a different number of times (duplicate layout).
    RowCount { name: String, expected: usize, got: usize },
    /// A frozen header's value differs at the given 0-based row.
    Value { name: String, row: usize, expected: String, got: String },
    /// A remote-target header's parameter differs at the given 0-based row
    /// (`None` = the param is absent on that side).
    Param {
        name: String,
        row: usize,
        param: String,
        expected: Option<String>,
        got: Option<String>,
    },
    /// The body differs: declared lengths and the first divergent byte offset
    /// (a length-only difference reports the offset at the shorter length).
    Body { expected_len: usize, got_len: usize, first_diff: Option<usize> },
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mismatch::StartLine { expected, got } => {
                write!(f, "start-line mismatch: expected {expected}, got {got}")
            }
            Mismatch::MissingHeader { name } => write!(f, "missing header {name:?}"),
            Mismatch::RowCount { name, expected, got } => write!(
                f,
                "header {name:?} row count mismatch: expected {expected}, got {got}"
            ),
            Mismatch::Value { name, row, expected, got } => write!(
                f,
                "header {name:?} row {row} value mismatch: expected {expected:?}, got {got:?}"
            ),
            Mismatch::Param { name, row, param, expected, got } => write!(
                f,
                "header {name:?} row {row} param {param:?} mismatch: expected {expected:?}, got {got:?}"
            ),
            Mismatch::Body { expected_len, got_len, first_diff } => write!(
                f,
                "body mismatch: expected {expected_len} bytes, got {got_len}, first diff at {first_diff:?}"
            ),
        }
    }
}

/// The canonical, case-folded key of a header name (compact forms expanded).
fn canon_key(name: &str) -> String {
    expand_compact_form(name).to_ascii_lowercase()
}

/// Extract a remote-target header's parameters (URI params + name-addr header
/// params) into a lowercase-keyed map. A flag param maps to an empty value.
fn remote_target_params(value: &str) -> BTreeMap<String, String> {
    let mut params: BTreeMap<String, String> = parse_sip_uri(value)
        .map(|u| u.params.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect())
        .unwrap_or_default();
    // Header parameters after a name-addr's closing '>'.
    if let Some(gt) = value.find('>') {
        for tok in value[gt + 1..].split(';').skip(1) {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            let (k, v) = match tok.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
                None => (tok.to_ascii_lowercase(), String::new()),
            };
            params.insert(k, v);
        }
    }
    params
}

/// Compare a remote-target header row by parameters modulo the ignore list.
fn compare_params(
    name: &str,
    row: usize,
    template_value: &str,
    inbound_value: &str,
    ignore: &[String],
) -> Option<Mismatch> {
    let mut tp = remote_target_params(template_value);
    let mut ip = remote_target_params(inbound_value);
    for k in ignore {
        tp.remove(k);
        ip.remove(k);
    }
    let mut keys: Vec<&String> = tp.keys().chain(ip.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        if tp.get(k) != ip.get(k) {
            return Some(Mismatch::Param {
                name: name.to_string(),
                row,
                param: k.clone(),
                expected: tp.get(k).cloned(),
                got: ip.get(k).cloned(),
            });
        }
    }
    None
}

impl MessageTemplate {
    /// Assert `msg` matches this template under the tier model (module doc).
    /// Returns the FIRST [`Mismatch`], or `Ok(())` on a full match.
    pub fn match_inbound(&self, msg: &SipMessage, opts: &MatchOpts) -> Result<(), Mismatch> {
        // Start line: kind + method (request) / status code (response).
        match (self.start(), msg) {
            (TemplateStart::Request(m), SipMessage::Request(r)) => {
                if r.method.as_str() != m.as_str() {
                    return Err(Mismatch::StartLine {
                        expected: m.as_str().to_string(),
                        got: r.method.as_str().to_string(),
                    });
                }
            }
            (TemplateStart::Response { status, .. }, SipMessage::Response(r)) => {
                if r.status != *status {
                    return Err(Mismatch::StartLine {
                        expected: status.to_string(),
                        got: r.status.to_string(),
                    });
                }
            }
            (TemplateStart::Request(m), SipMessage::Response(r)) => {
                return Err(Mismatch::StartLine {
                    expected: format!("request {}", m.as_str()),
                    got: format!("response {}", r.status),
                })
            }
            (TemplateStart::Response { status, .. }, SipMessage::Request(r)) => {
                return Err(Mismatch::StartLine {
                    expected: format!("response {status}"),
                    got: format!("request {}", r.method.as_str()),
                })
            }
        }

        let ignore: Vec<String> =
            opts.ignore_params.iter().map(|s| s.to_ascii_lowercase()).collect();
        let inbound = msg.headers();

        // Distinct canonical header names, in template order.
        let mut seen: Vec<String> = Vec::new();
        for h in self.headers() {
            let key = canon_key(&h.name);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }

        for key in &seen {
            let t_rows: Vec<&str> = self
                .headers()
                .iter()
                .filter(|h| canon_key(&h.name) == *key)
                .map(|h| h.value.as_str())
                .collect();
            let i_rows: Vec<&str> = inbound
                .iter()
                .filter(|h| canon_key(&h.name) == *key)
                .map(|h| h.value.as_str())
                .collect();

            if TIER1.contains(&key.as_str()) {
                // Structural only: present (syntax already guaranteed by parse).
                if i_rows.is_empty() {
                    return Err(Mismatch::MissingHeader { name: key.clone() });
                }
                continue;
            }

            if t_rows.len() != i_rows.len() {
                return Err(Mismatch::RowCount {
                    name: key.clone(),
                    expected: t_rows.len(),
                    got: i_rows.len(),
                });
            }

            if REMOTE_TARGET.contains(&key.as_str()) {
                for (row, (tv, iv)) in t_rows.iter().zip(&i_rows).enumerate() {
                    if let Some(m) = compare_params(key, row, tv, iv, &ignore) {
                        return Err(m);
                    }
                }
            } else {
                // Frozen: byte-wise value comparison (parsed values).
                for (row, (tv, iv)) in t_rows.iter().zip(&i_rows).enumerate() {
                    if tv != iv {
                        return Err(Mismatch::Value {
                            name: key.clone(),
                            row,
                            expected: (*tv).to_string(),
                            got: (*iv).to_string(),
                        });
                    }
                }
            }
        }

        // Body: compared byte-wise only when the template carries one.
        let tb = self.body();
        if !tb.is_empty() {
            let ib = match msg {
                SipMessage::Request(r) => &r.body,
                SipMessage::Response(r) => &r.body,
            };
            if tb != ib.as_slice() {
                let first_diff = tb.iter().zip(ib.iter()).position(|(a, b)| a != b).or({
                    if tb.len() != ib.len() {
                        Some(tb.len().min(ib.len()))
                    } else {
                        None
                    }
                });
                return Err(Mismatch::Body {
                    expected_len: tb.len(),
                    got_len: ib.len(),
                    first_diff,
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::parser::SipParser;
    use crate::template::{MessageTemplate, TemplateHeader};

    fn parse(raw: &[u8]) -> SipMessage {
        CustomParser::new().parse(raw).expect("well-formed")
    }

    /// A captured INVITE with a frozen extension header + a body.
    fn captured_invite(subject: &str, body: &str) -> SipMessage {
        let raw = format!(
            "INVITE sip:b@ex SIP/2.0\r\n\
Via: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-cap\r\n\
Max-Forwards: 70\r\n\
From: <sip:a@ex>;tag=at\r\n\
To: <sip:b@ex>\r\n\
Call-ID: cid@1.1.1.1\r\n\
CSeq: 3 INVITE\r\n\
Contact: <sip:a@1.1.1.1:5060>\r\n\
Subject: {}\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
            subject,
            body.len(),
            body,
        );
        parse(raw.as_bytes())
    }

    #[test]
    fn full_match_passes_despite_regenerated_tier1() {
        // The inbound has DIFFERENT tier-1 values (fresh branch/Call-ID/tags/
        // CSeq) but matching frozen Subject + body — a full match.
        let tmpl = MessageTemplate::from_message(&captured_invite("hi", "sdp-body"));
        let inbound = {
            let raw = "INVITE sip:b@other SIP/2.0\r\n\
Via: SIP/2.0/UDP 9.9.9.9:5061;branch=z9hG4bK-fresh\r\n\
Max-Forwards: 70\r\n\
From: <sip:x@ex>;tag=freshtag\r\n\
To: <sip:b@ex>\r\n\
Call-ID: fresh@9.9.9.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:x@9.9.9.9:5061>\r\n\
Allow: INVITE, ACK, BYE\r\n\
Subject: hi\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 8\r\n\r\nsdp-body";
            parse(raw.as_bytes())
        };
        assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));
    }

    #[test]
    fn frozen_value_drift_is_caught_precisely() {
        let tmpl = MessageTemplate::from_message(&captured_invite("expected-subject", "b"));
        let inbound = captured_invite("changed-subject", "b");
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::Value { name, row, expected, got }) => {
                assert_eq!(name, "subject");
                assert_eq!(row, 0);
                assert_eq!(expected, "expected-subject");
                assert_eq!(got, "changed-subject");
            }
            other => panic!("expected a Value mismatch, got {other:?}"),
        }
    }

    #[test]
    fn compact_and_full_names_match_both_directions() {
        // Template captured compact `k:`; inbound full `Supported:` → match.
        let tmpl = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("k", "replaces")],
            Vec::new(),
        );
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nSupported: replaces\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));

        // Template captured full `Supported:`; inbound compact `k:` → match.
        let tmpl2 = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("Supported", "replaces")],
            Vec::new(),
        );
        let inbound2 = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nk: replaces\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(tmpl2.match_inbound(&inbound2, &MatchOpts::default()), Ok(()));
    }

    #[test]
    fn duplicate_row_count_mismatch_is_caught() {
        let tmpl = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("X-Trace", "a"), TemplateHeader::frozen("X-Trace", "b")],
            Vec::new(),
        );
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nX-Trace: a\r\nContent-Length: 0\r\n\r\n",
        );
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::RowCount { name, expected, got }) => {
                assert_eq!(name, "x-trace");
                assert_eq!(expected, 2);
                assert_eq!(got, 1);
            }
            other => panic!("expected RowCount, got {other:?}"),
        }
    }

    #[test]
    fn body_drift_reports_first_offset() {
        let tmpl = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("Content-Type", "text/plain")],
            b"hello world".to_vec(),
        );
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContent-Type: text/plain\r\nContent-Length: 11\r\n\r\nhello WORLD",
        );
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::Body { expected_len, got_len, first_diff }) => {
                assert_eq!(expected_len, 11);
                assert_eq!(got_len, 11);
                assert_eq!(first_diff, Some(6), "first divergent byte at 'w' vs 'W'");
            }
            other => panic!("expected Body, got {other:?}"),
        }
    }

    #[test]
    fn start_line_status_mismatch_is_caught() {
        // The pilot-defect shape: expected a 400, got a 200.
        let tmpl =
            MessageTemplate::response(400, "Bad Request", vec![], Vec::new());
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 2 UPDATE\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(
            tmpl.match_inbound(&inbound, &MatchOpts::default()),
            Err(Mismatch::StartLine { expected: "400".into(), got: "200".into() }),
        );
    }

    // --- remote-target benign-param tolerance (generic LB-style fixtures) ------

    fn response_with_contact(contact: &str) -> SipMessage {
        let raw = format!(
            "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContact: {contact}\r\nContent-Length: 0\r\n\r\n"
        );
        parse(raw.as_bytes())
    }

    #[test]
    fn remote_target_added_param_tolerated_only_when_ignored() {
        // Template Contact has a `user` param; the inbound Contact additionally
        // carries an environment-added `primary=<id>` (an LB stamp).
        let tmpl = MessageTemplate::from_message(&response_with_contact(
            "<sip:b@1.1.1.1:5060;user=phone>",
        ));
        let inbound = response_with_contact("<sip:b@1.1.1.1:5060;user=phone;primary=lb1>");

        // Without ignoring it, the extra param fails with param-level detail.
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::Param { name, param, expected, got, .. }) => {
                assert_eq!(name, "contact");
                assert_eq!(param, "primary");
                assert_eq!(expected, None);
                assert_eq!(got, Some("lb1".to_string()));
            }
            other => panic!("expected a Param mismatch, got {other:?}"),
        }

        // Ignoring `primary` (and `backup`) tolerates it.
        assert_eq!(
            tmpl.match_inbound(&inbound, &MatchOpts::ignoring(["primary", "backup"])),
            Ok(()),
        );
    }

    #[test]
    fn remote_target_non_ignored_param_drift_fails_with_detail() {
        let tmpl = MessageTemplate::from_message(&response_with_contact(
            "<sip:b@1.1.1.1:5060;user=phone>",
        ));
        // A meaningful (non-ignored) param drift: user=phone -> user=isdn.
        let inbound = response_with_contact("<sip:b@1.1.1.1:5060;user=isdn>");
        match tmpl.match_inbound(&inbound, &MatchOpts::ignoring(["primary", "backup"])) {
            Err(Mismatch::Param { name, param, expected, got, .. }) => {
                assert_eq!(name, "contact");
                assert_eq!(param, "user");
                assert_eq!(expected, Some("phone".to_string()));
                assert_eq!(got, Some("isdn".to_string()));
            }
            other => panic!("expected a Param mismatch, got {other:?}"),
        }
    }
}
