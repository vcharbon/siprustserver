//! Tiered expectation matching — assert an INBOUND parsed message against a
//! [`MessageTemplate`], returning a structured [`Mismatch`] on the first
//! divergence.
//!
//! Classification is by the template header's [`HeaderClass`] (the SAME source
//! emission uses), so an explicit class override is honoured on both sides:
//!
//! - **Frozen** (explicit, or by default `Content-Type` + every extension
//!   header): value-compared byte-wise against the template's parsed value.
//! - **Regenerated** headers dispatch by canonical name:
//!   - **tier-1 / structural** (`Via`, `From`, `To`, `Call-ID`, `CSeq`,
//!     `Max-Forwards`, `Content-Length`): the live dialog owns them, so they are
//!     NOT value-compared; presence is required only for the RFC-mandatory core
//!     (`Via`/`From`/`To`/`Call-ID`/`CSeq`) — a legal missing `Content-Length`
//!     (RFC 3261 §20.14) does not fail.
//!   - **remote-target** (`Contact`/`Route`/`Record-Route`): the routing-critical
//!     part is host:port ONLY. Per element, the user part is compared byte-wise
//!     and the URI params and header params are compared SEPARATELY (each
//!     order-insensitive, modulo [`MatchOpts::ignore_params`]); host:port is
//!     ignored. This mirrors emission ([`crate::remote_target`]), so a
//!     param'd/user'd Contact round-trips.
//!
//! Header NAME comparison is canonical (a captured compact `k:` matches an
//! inbound `Supported:` and vice versa); duplicate rows must match in count and
//! per-row order; the start line compares method (request) or status code AND
//! reason phrase (response); the body is compared byte-wise (a bodiless template
//! requires a bodiless inbound). The match is TEMPLATE-DRIVEN: inbound headers
//! the template does not mention (a stack-added `Allow`/`Date`) are not flagged.

use std::collections::BTreeMap;

use crate::remote_target::{canonical, is_remote_target, parse_elements, RtElement};
use crate::template::{HeaderClass, MessageTemplate, TemplateHeader, TemplateStart};
use crate::types::SipMessage;

/// Tier-1 headers the live dialog owns — never value-compared.
const TIER1: &[&str] =
    &["via", "from", "to", "call-id", "cseq", "max-forwards", "content-length"];

/// The subset of tier-1 whose presence is RFC-mandatory (and parser-guaranteed)
/// for both requests and responses — the only headers whose absence is a
/// mismatch. `Max-Forwards` (request-only) and `Content-Length` (§20.14
/// optional) are deliberately excluded.
const MANDATORY: &[&str] = &["via", "from", "to", "call-id", "cseq"];

/// Options controlling [`MessageTemplate::match_inbound`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchOpts {
    /// Parameter NAMES excluded from comparison on remote-target headers
    /// (`Contact`/`Route`/`Record-Route`), on BOTH sides and for BOTH the URI
    /// and header parameter sets — benign environment-added params (LB ids,
    /// etc.). Case-insensitive. Params not listed compare exactly.
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
    /// The response reason phrase differs.
    ReasonPhrase { expected: String, got: String },
    /// A mandatory tier-1 header the template carries is absent from the inbound.
    MissingHeader { name: String },
    /// A compared header appears a different number of times (duplicate layout).
    RowCount { name: String, expected: usize, got: usize },
    /// A remote-target header row has a different number of comma elements.
    ElementCount { name: String, row: usize, expected: usize, got: usize },
    /// A frozen header's value differs at the given 0-based row.
    Value { name: String, row: usize, expected: String, got: String },
    /// A remote-target element's user part differs (row/element 0-based).
    UserPart {
        name: String,
        row: usize,
        element: usize,
        expected: Option<String>,
        got: Option<String>,
    },
    /// A remote-target element's parameter differs (`kind` = "uri" | "header";
    /// `None` = absent on that side).
    Param {
        name: String,
        row: usize,
        element: usize,
        kind: &'static str,
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
            Mismatch::ReasonPhrase { expected, got } => {
                write!(f, "reason-phrase mismatch: expected {expected:?}, got {got:?}")
            }
            Mismatch::MissingHeader { name } => write!(f, "missing header {name:?}"),
            Mismatch::RowCount { name, expected, got } => {
                write!(f, "header {name:?} row count mismatch: expected {expected}, got {got}")
            }
            Mismatch::ElementCount { name, row, expected, got } => write!(
                f,
                "header {name:?} row {row} element count mismatch: expected {expected}, got {got}"
            ),
            Mismatch::Value { name, row, expected, got } => write!(
                f,
                "header {name:?} row {row} value mismatch: expected {expected:?}, got {got:?}"
            ),
            Mismatch::UserPart { name, row, element, expected, got } => write!(
                f,
                "header {name:?} row {row} element {element} user-part mismatch: expected {expected:?}, got {got:?}"
            ),
            Mismatch::Param { name, row, element, kind, param, expected, got } => write!(
                f,
                "header {name:?} row {row} element {element} {kind}-param {param:?} mismatch: expected {expected:?}, got {got:?}"
            ),
            Mismatch::Body { expected_len, got_len, first_diff } => write!(
                f,
                "body mismatch: expected {expected_len} bytes, got {got_len}, first diff at {first_diff:?}"
            ),
        }
    }
}

/// Compare two parameter maps modulo the ignore list; return the first divergent
/// param `(name, expected, got)` or `None`.
fn diff_params(
    template: &BTreeMap<String, String>,
    inbound: &BTreeMap<String, String>,
    ignore: &[String],
) -> Option<(String, Option<String>, Option<String>)> {
    let mut keys: Vec<&String> = template.keys().chain(inbound.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        if ignore.iter().any(|i| i == k) {
            continue;
        }
        if template.get(k) != inbound.get(k) {
            return Some((k.clone(), template.get(k).cloned(), inbound.get(k).cloned()));
        }
    }
    None
}

/// Compare one remote-target row (user part + URI/header params per element,
/// host:port ignored).
fn compare_remote_target(
    name: &str,
    row: usize,
    template_value: &str,
    inbound_value: &str,
    ignore: &[String],
) -> Option<Mismatch> {
    let t_elems = parse_elements(template_value);
    let i_elems = parse_elements(inbound_value);
    if t_elems.len() != i_elems.len() {
        return Some(Mismatch::ElementCount {
            name: name.to_string(),
            row,
            expected: t_elems.len(),
            got: i_elems.len(),
        });
    }
    for (element, (te, ie)) in t_elems.iter().zip(&i_elems).enumerate() {
        if let Some(m) = compare_element(name, row, element, te, ie, ignore) {
            return Some(m);
        }
    }
    None
}

fn compare_element(
    name: &str,
    row: usize,
    element: usize,
    te: &RtElement,
    ie: &RtElement,
    ignore: &[String],
) -> Option<Mismatch> {
    if te.user != ie.user {
        return Some(Mismatch::UserPart {
            name: name.to_string(),
            row,
            element,
            expected: te.user.clone(),
            got: ie.user.clone(),
        });
    }
    // URI params and header params compared SEPARATELY (a placement move fails).
    if let Some((param, expected, got)) = diff_params(&te.uri_params, &ie.uri_params, ignore) {
        return Some(Mismatch::Param {
            name: name.to_string(),
            row,
            element,
            kind: "uri",
            param,
            expected,
            got,
        });
    }
    if let Some((param, expected, got)) = diff_params(&te.header_params, &ie.header_params, ignore)
    {
        return Some(Mismatch::Param {
            name: name.to_string(),
            row,
            element,
            kind: "header",
            param,
            expected,
            got,
        });
    }
    None
}

/// The effective match class of a template header name group (first row's class;
/// `from_message` assigns one class per name).
fn effective_class(rows: &[&TemplateHeader]) -> HeaderClass {
    rows.first().map(|h| h.class).unwrap_or(HeaderClass::Frozen)
}

impl MessageTemplate {
    /// Assert `msg` matches this template under the tier model (module doc).
    /// Returns the FIRST [`Mismatch`], or `Ok(())` on a full match.
    // `Mismatch` is a rich diagnostic value, deliberately returned by-value (not
    // boxed) so it reads directly in an assertion / test failure.
    #[allow(clippy::result_large_err)]
    pub fn match_inbound(&self, msg: &SipMessage, opts: &MatchOpts) -> Result<(), Mismatch> {
        // Start line: kind + method (request) / status code + reason (response).
        match (self.start(), msg) {
            (TemplateStart::Request(m), SipMessage::Request(r)) => {
                if r.method.as_str() != m.as_str() {
                    return Err(Mismatch::StartLine {
                        expected: m.as_str().to_string(),
                        got: r.method.as_str().to_string(),
                    });
                }
            }
            (TemplateStart::Response { status, reason }, SipMessage::Response(r)) => {
                if r.status != *status {
                    return Err(Mismatch::StartLine {
                        expected: status.to_string(),
                        got: r.status.to_string(),
                    });
                }
                if r.reason != *reason {
                    return Err(Mismatch::ReasonPhrase {
                        expected: reason.clone(),
                        got: r.reason.clone(),
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
            let key = canonical(&h.name);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }

        for key in &seen {
            let t_headers: Vec<&TemplateHeader> =
                self.headers().iter().filter(|h| canonical(&h.name) == *key).collect();
            let t_rows: Vec<&str> = t_headers.iter().map(|h| h.value.as_str()).collect();
            let i_rows: Vec<&str> = inbound
                .iter()
                .filter(|h| canonical(&h.name) == *key)
                .map(|h| h.value.as_str())
                .collect();

            let class = effective_class(&t_headers);

            // Regenerated tier-1: structural only (presence for the mandatory core).
            if class == HeaderClass::Regenerated
                && TIER1.contains(&key.as_str())
                && !is_remote_target(key)
            {
                if MANDATORY.contains(&key.as_str()) && i_rows.is_empty() {
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

            // Regenerated remote-target: user + params (host:port ignored).
            if class == HeaderClass::Regenerated && is_remote_target(key) {
                for (row, (tv, iv)) in t_rows.iter().zip(&i_rows).enumerate() {
                    if let Some(m) = compare_remote_target(key, row, tv, iv, &ignore) {
                        return Err(m);
                    }
                }
                continue;
            }

            // Frozen (explicit override, or a non-tier-1/non-remote-target
            // Regenerated header): byte-wise value comparison.
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

        // Body: byte-wise, symmetric — a bodiless template requires a bodiless
        // inbound (no opt-out in v1).
        let tb = self.body();
        let ib = match msg {
            SipMessage::Request(r) => r.body.as_slice(),
            SipMessage::Response(r) => r.body.as_slice(),
        };
        if tb != ib {
            let first_diff = tb.iter().zip(ib.iter()).position(|(a, b)| a != b).or({
                if tb.len() != ib.len() {
                    Some(tb.len().min(ib.len()))
                } else {
                    None
                }
            });
            return Err(Mismatch::Body { expected_len: tb.len(), got_len: ib.len(), first_diff });
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
        let tmpl = MessageTemplate::from_message(&captured_invite("hi", "sdp-body"));
        let inbound = parse(
            b"INVITE sip:b@other SIP/2.0\r\n\
Via: SIP/2.0/UDP 9.9.9.9:5061;branch=z9hG4bK-fresh\r\n\
Max-Forwards: 70\r\n\
From: <sip:x@ex>;tag=freshtag\r\n\
To: <sip:b@ex>\r\n\
Call-ID: fresh@9.9.9.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:a@9.9.9.9:5061>\r\n\
Allow: INVITE, ACK, BYE\r\n\
Subject: hi\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 8\r\n\r\nsdp-body",
        );
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
                assert_eq!(first_diff, Some(6));
            }
            other => panic!("expected Body, got {other:?}"),
        }
    }

    #[test]
    fn bodiless_template_rejects_inbound_with_body() {
        let tmpl = MessageTemplate::response(200, "OK", vec![], Vec::new());
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\nxyz",
        );
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::Body { expected_len, got_len, .. }) => {
                assert_eq!(expected_len, 0);
                assert_eq!(got_len, 3);
            }
            other => panic!("expected Body, got {other:?}"),
        }
    }

    #[test]
    fn start_line_status_and_reason_mismatch() {
        // Status mismatch (the pilot 400-vs-200 shape).
        let tmpl = MessageTemplate::response(400, "Bad Request", vec![], Vec::new());
        let got_200 = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 2 UPDATE\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(
            tmpl.match_inbound(&got_200, &MatchOpts::default()),
            Err(Mismatch::StartLine { expected: "400".into(), got: "200".into() }),
        );

        // Reason-phrase mismatch — the distinctive defect signature.
        let tmpl2 =
            MessageTemplate::response(400, "Bad Request - Invalid Dialog State", vec![], Vec::new());
        let got_plain_400 = parse(
            b"SIP/2.0 400 Bad Request\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 2 UPDATE\r\nContent-Length: 0\r\n\r\n",
        );
        assert_eq!(
            tmpl2.match_inbound(&got_plain_400, &MatchOpts::default()),
            Err(Mismatch::ReasonPhrase {
                expected: "Bad Request - Invalid Dialog State".into(),
                got: "Bad Request".into(),
            }),
        );
    }

    #[test]
    fn missing_content_length_is_tolerated() {
        // A template with Content-Length matched against an inbound WITHOUT one
        // (RFC 3261 §20.14 legal) passes — Content-Length presence is not required.
        let tmpl = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("X-Note", "n")],
            Vec::new(),
        );
        let inbound = parse(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nX-Note: n\r\n\r\n",
        );
        assert_eq!(tmpl.match_inbound(&inbound, &MatchOpts::default()), Ok(()));
    }

    // --- remote-target: user part + params (host:port ignored) -----------------

    fn response_with_contact(contact: &str) -> SipMessage {
        let raw = format!(
            "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 1.1.1.1:5060;branch=z9hG4bK-1\r\n\
From: <sip:a@ex>;tag=at\r\nTo: <sip:b@ex>;tag=bt\r\nCall-ID: c@1\r\n\
CSeq: 1 INVITE\r\nContact: {contact}\r\nContent-Length: 0\r\n\r\n"
        );
        parse(raw.as_bytes())
    }

    #[test]
    fn remote_target_hostport_ignored_but_user_compared() {
        // Same user + params, DIFFERENT host:port → match (host:port ignored).
        let tmpl = MessageTemplate::from_message(&response_with_contact(
            "<sip:bob@1.1.1.1:5060;user=phone>",
        ));
        let diff_hostport = response_with_contact("<sip:bob@9.9.9.9:5070;user=phone>");
        assert_eq!(tmpl.match_inbound(&diff_hostport, &MatchOpts::default()), Ok(()));

        // Different USER part → caught precisely.
        let diff_user = response_with_contact("<sip:totally-different@9.9.9.9:5070;user=phone>");
        match tmpl.match_inbound(&diff_user, &MatchOpts::default()) {
            Err(Mismatch::UserPart { name, expected, got, .. }) => {
                assert_eq!(name, "contact");
                assert_eq!(expected, Some("bob".to_string()));
                assert_eq!(got, Some("totally-different".to_string()));
            }
            other => panic!("expected a UserPart mismatch, got {other:?}"),
        }
    }

    #[test]
    fn remote_target_added_param_tolerated_only_when_ignored() {
        let tmpl = MessageTemplate::from_message(&response_with_contact(
            "<sip:b@1.1.1.1:5060;user=phone>",
        ));
        let inbound = response_with_contact("<sip:b@1.1.1.1:5060;user=phone;primary=lb1>");
        match tmpl.match_inbound(&inbound, &MatchOpts::default()) {
            Err(Mismatch::Param { name, kind, param, expected, got, .. }) => {
                assert_eq!(name, "contact");
                assert_eq!(kind, "uri");
                assert_eq!(param, "primary");
                assert_eq!(expected, None);
                assert_eq!(got, Some("lb1".to_string()));
            }
            other => panic!("expected a Param mismatch, got {other:?}"),
        }
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

    #[test]
    fn remote_target_param_placement_move_fails() {
        // `expires` is a URI param on the template but a header param on the
        // inbound — the two sets compare separately, so this fails.
        let tmpl =
            MessageTemplate::from_message(&response_with_contact("<sip:b@h:5060;expires=60>"));
        let moved = response_with_contact("<sip:b@h:5060>;expires=60");
        match tmpl.match_inbound(&moved, &MatchOpts::default()) {
            Err(Mismatch::Param { kind, param, .. }) => {
                assert_eq!(kind, "uri");
                assert_eq!(param, "expires");
            }
            other => panic!("expected a Param mismatch (placement move), got {other:?}"),
        }
    }

    #[test]
    fn frozen_contact_override_is_byte_compared() {
        // An explicitly FROZEN Contact is byte-compared whole (host:port too).
        let tmpl = MessageTemplate::response(
            200,
            "OK",
            vec![TemplateHeader::frozen("Contact", "<sip:b@1.1.1.1:5060>")],
            Vec::new(),
        );
        let same = response_with_contact("<sip:b@1.1.1.1:5060>");
        assert_eq!(tmpl.match_inbound(&same, &MatchOpts::default()), Ok(()));
        let diff = response_with_contact("<sip:b@9.9.9.9:5070>");
        assert!(matches!(
            tmpl.match_inbound(&diff, &MatchOpts::default()),
            Err(Mismatch::Value { .. })
        ));
    }
}
