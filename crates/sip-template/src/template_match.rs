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
//!     ignored. This mirrors emission ([`sip_message::remote_target`]), so a
//!     param'd/user'd Contact round-trips.
//!
//! Header NAME comparison is canonical (a captured compact `k:` matches an
//! inbound `Supported:` and vice versa); duplicate rows must match in count and
//! per-row order; the start line compares method (request) or status code AND
//! reason phrase (response); the body is compared byte-wise (a bodiless template
//! requires a bodiless inbound). The match is TEMPLATE-DRIVEN: inbound headers
//! the template does not mention (a stack-added `Allow`/`Date`) are not flagged.

use std::collections::BTreeMap;

use crate::template::{HeaderClass, MessageTemplate, TemplateHeader, TemplateStart};
use sip_message::remote_target::{canonical, is_remote_target, parse_elements, RtElement};
use sip_message::types::SipMessage;

/// Tier-1 headers the live dialog owns — never value-compared.
const TIER1: &[&str] = &["via", "from", "to", "call-id", "cseq", "max-forwards", "content-length"];

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
    rows.first().map(|h| h.class).unwrap_or(HeaderClass::EndToEnd)
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
                if r.method().as_str() != m.as_str() {
                    return Err(Mismatch::StartLine {
                        expected: m.as_str().to_string(),
                        got: r.method().as_str().to_string(),
                    });
                }
            }
            (TemplateStart::Response { status, reason }, SipMessage::Response(r)) => {
                if r.status() != *status {
                    return Err(Mismatch::StartLine {
                        expected: status.to_string(),
                        got: r.status().to_string(),
                    });
                }
                if r.reason() != *reason {
                    return Err(Mismatch::ReasonPhrase {
                        expected: reason.clone(),
                        got: r.reason().to_string(),
                    });
                }
            }
            (TemplateStart::Request(m), SipMessage::Response(r)) => {
                return Err(Mismatch::StartLine {
                    expected: format!("request {}", m.as_str()),
                    got: format!("response {}", r.status()),
                })
            }
            (TemplateStart::Response { status, .. }, SipMessage::Request(r)) => {
                return Err(Mismatch::StartLine {
                    expected: format!("response {status}"),
                    got: format!("request {}", r.method().as_str()),
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
            if class == HeaderClass::Structural
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
            if class == HeaderClass::Structural && is_remote_target(key) {
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
            SipMessage::Request(r) => r.body(),
            SipMessage::Response(r) => r.body(),
        };
        if tb != &ib[..] {
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
mod tests;
