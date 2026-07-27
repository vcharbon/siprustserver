//! The **check engine** (ADR-0019, Phase E): evaluate a Test case's declarative
//! checks post-call over the recorded trace — the same recording the RFC audit
//! rules gate — so fake and real infra shapes yield byte-identical verdicts.
//!
//! Resolution: `<agent>.<anchor>` → the [`AnchorTag`] the Callflow shape
//! attached at receive time → the first delivered [`RecordedSipEntry`] to that
//! agent whose re-parsed bytes match the tag's identity keys. A non-`optional`
//! block whose selector resolves to no recorded message **fails loudly**.
//!
//! Field grammar: URI-bearing headers (`from`/`to`/`ruri`/`pai`/`ppi`/
//! `diversion`/`contact`, list ones indexable as `pai[1]`) expose
//! `.userInfo/.host/.port/.displayName/.tag/.param(x)/.uri`; any other header
//! is `header(Name)` (raw values, comma-joined); the payload is `body`; the
//! transport endpoints are `source.ip/.port` / `dest.ip/.port` from the
//! recorded addresses. Values may bind `${input.<field>}` / `${infra.lbVip}`.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use scenario_harness::{AnchorTag, RunReport};
use sip_message::header::kind::NameAddrKind;
use sip_message::header::{self, HeaderName, NameAddr, NameAddrHeader, ParamValue, Uri};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParseError, SipParser};
use sip_net::RecordedSipEntry;

use crate::model::{Check, CheckBlock, CheckOp, CheckSet, Input, TestCase};

/// The outcome of one [`Check`], or of a whole block that failed/skipped at
/// resolution (then `field` is `(anchor)`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckVerdict {
    /// The block's `<agent>.<anchor>` selector.
    pub on: String,
    pub field: String,
    pub op: CheckOp,
    /// The expected value after `${…}` substitution (when the op takes one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The extracted value (`None` = field absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    pub passed: bool,
    /// Human-readable explanation (why it failed, or that it was skipped).
    pub detail: String,
}

/// Value bindings available to `${…}` substitution in check values.
pub struct Bindings<'a> {
    pub input: &'a Input,
    /// `${infra.lbVip}` — the Infra shape's LB VIP.
    pub lb_vip: SocketAddr,
}

/// Evaluate every check a Test case binds (inline blocks + its Check sets)
/// against a finished run. Unknown check-set ids yield failed verdicts (the
/// loader validation catches them earlier; this is the run-time backstop).
pub fn evaluate_case(
    case: &TestCase,
    check_sets: &BTreeMap<String, CheckSet>,
    report: &RunReport,
    bindings: &Bindings<'_>,
) -> Vec<CheckVerdict> {
    evaluate_case_over(case, check_sets, &report.entries(), report.anchors(), bindings)
}

/// [`evaluate_case`] over a raw `(entries, anchors)` pair instead of a
/// [`RunReport`] — the ONE engine both run surfaces share: the e2e executor
/// passes its report's projection; the load driver passes a sampled call's
/// `AgentBinder::recorded_entries()` + `CallCtx::take_anchors()`.
pub fn evaluate_case_over(
    case: &TestCase,
    check_sets: &BTreeMap<String, CheckSet>,
    entries: &[RecordedSipEntry],
    anchors: &[AnchorTag],
    bindings: &Bindings<'_>,
) -> Vec<CheckVerdict> {
    let mut verdicts = Vec::new();
    let mut blocks: Vec<&CheckBlock> = case.checks.iter().collect();
    for set_id in &case.check_sets {
        match check_sets.get(set_id) {
            Some(set) => blocks.extend(set.blocks.iter()),
            None => verdicts.push(CheckVerdict {
                on: set_id.clone(),
                field: "(check set)".into(),
                op: CheckOp::Exists,
                expected: None,
                actual: None,
                passed: false,
                detail: format!("unknown check set {set_id:?}"),
            }),
        }
    }
    verdicts.extend(evaluate_blocks_over(&blocks, entries, anchors, bindings));
    verdicts
}

/// `true` iff every verdict passed — the cell-verdict fold for the checks half
/// (RFC findings are gated by the harness itself).
pub fn all_passed(verdicts: &[CheckVerdict]) -> bool {
    verdicts.iter().all(|v| v.passed)
}

/// Evaluate explicit blocks against a finished run.
pub fn evaluate_blocks(
    blocks: &[&CheckBlock],
    report: &RunReport,
    bindings: &Bindings<'_>,
) -> Vec<CheckVerdict> {
    evaluate_blocks_over(blocks, &report.entries(), report.anchors(), bindings)
}

/// [`evaluate_blocks`] over a raw `(entries, anchors)` pair (see
/// [`evaluate_case_over`]).
pub fn evaluate_blocks_over(
    blocks: &[&CheckBlock],
    entries: &[RecordedSipEntry],
    anchors: &[AnchorTag],
    bindings: &Bindings<'_>,
) -> Vec<CheckVerdict> {
    // Sort like the renderers: capture order is (sent_ms, seq).
    let mut entries = entries.to_vec();
    entries.sort_by(|a, b| a.sent_ms.cmp(&b.sent_ms).then(a.seq.cmp(&b.seq)));

    let mut verdicts = Vec::new();
    for block in blocks {
        match resolve(block, anchors, &entries) {
            Ok((entry, msg)) => {
                for check in &block.checks {
                    verdicts.push(evaluate_check(block, check, entry, &msg, bindings));
                }
            }
            Err(detail) => verdicts.push(CheckVerdict {
                on: block.on.clone(),
                field: "(anchor)".into(),
                op: CheckOp::Exists,
                expected: None,
                actual: None,
                passed: block.optional,
                detail: if block.optional {
                    format!("skipped (optional): {detail}")
                } else {
                    detail
                },
            }),
        }
    }
    verdicts
}

/// `<agent>.<anchor>` → the recorded message: the tag the shape attached, then
/// the first delivered entry to that agent matching the tag's identity keys
/// (or FROM it, for a `sent` tag — a message whose only receiver is the SUT).
fn resolve<'e>(
    block: &CheckBlock,
    anchors: &[AnchorTag],
    entries: &'e [RecordedSipEntry],
) -> Result<(&'e RecordedSipEntry, SipMessage), String> {
    let Some((agent, anchor)) = block.selector() else {
        return Err(format!("check selector {:?} must be `<agent>.<anchor>`", block.on));
    };
    let Some(tag) = anchors.iter().find(|t| t.agent == agent && t.anchor == anchor) else {
        let tagged = anchors
            .iter()
            .map(|t| format!("{}.{}", t.agent, t.anchor))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "no anchor {:?} was tagged by the shape (tagged: [{tagged}])",
            block.on
        ));
    };
    let parser = CustomParser::new();
    for e in entries {
        let addr_hit = if tag.sent { e.from == tag.agent_addr } else { e.to == tag.agent_addr };
        if !e.delivered || !addr_hit {
            continue;
        }
        if let Ok(msg) = parser.parse(&e.raw) {
            if tag.matches(&msg) {
                return Ok((e, msg));
            }
        }
    }
    Err(format!(
        "anchor {:?} was tagged but no delivered recorded message {} {} matches its keys",
        block.on,
        if tag.sent { "from" } else { "to" },
        tag.agent_addr
    ))
}

fn evaluate_check(
    block: &CheckBlock,
    check: &Check,
    entry: &RecordedSipEntry,
    msg: &SipMessage,
    bindings: &Bindings<'_>,
) -> CheckVerdict {
    let mut verdict = CheckVerdict {
        on: block.on.clone(),
        field: check.field.clone(),
        op: check.op,
        expected: None,
        actual: None,
        passed: false,
        detail: String::new(),
    };

    let expected = match &check.value {
        Some(v) => match substitute(v, bindings) {
            Ok(s) => Some(s),
            Err(e) => {
                verdict.detail = e;
                return verdict;
            }
        },
        None => None,
    };
    verdict.expected = expected.clone();

    let actual = match extract(&check.field, entry, msg) {
        Ok(a) => a,
        Err(e) => {
            verdict.detail = e;
            return verdict;
        }
    };
    verdict.actual = actual.clone();

    let (passed, detail) = match (check.op, &expected, &actual) {
        (CheckOp::Exists, _, Some(_)) => (true, "present".to_string()),
        (CheckOp::Exists, _, None) => (false, "expected present, was absent".to_string()),
        (CheckOp::Absent, _, None) => (true, "absent".to_string()),
        (CheckOp::Absent, _, Some(a)) => (false, format!("expected absent, was {a:?}")),
        (CheckOp::Eq, Some(want), Some(got)) => {
            if want == got {
                (true, "equal".to_string())
            } else {
                (false, format!("expected {want:?}, got {got:?}"))
            }
        }
        (CheckOp::Eq, Some(want), None) => (false, format!("expected {want:?}, field absent")),
        (CheckOp::Regex, Some(pat), got) => match regex::Regex::new(pat) {
            Err(e) => (false, format!("invalid regex {pat:?}: {e}")),
            Ok(re) => match got {
                Some(g) if re.is_match(g) => (true, format!("matches {pat:?}")),
                Some(g) => (false, format!("{g:?} does not match {pat:?}")),
                None => (false, format!("field absent, cannot match {pat:?}")),
            },
        },
        // value-presence is enforced by load-time validation; backstop here.
        (CheckOp::Eq | CheckOp::Regex, None, _) => {
            (false, format!("op {:?} requires a value", check.op))
        }
    };
    verdict.passed = passed;
    verdict.detail = detail;
    verdict
}

/// Replace every `${…}` token: `${input.from|to|ruri}`, `${input.<extras key>}`,
/// `${infra.lbVip}`. An unknown binding is a hard error (a typo must not pass).
fn substitute(value: &str, bindings: &Bindings<'_>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unterminated ${{…}} binding in {value:?}"));
        };
        let token = &after[..end];
        let resolved = match token {
            "infra.lbVip" => Some(bindings.lb_vip.to_string()),
            "infra.lbVip.ip" => Some(bindings.lb_vip.ip().to_string()),
            "infra.lbVip.port" => Some(bindings.lb_vip.port().to_string()),
            _ => match token.strip_prefix("input.") {
                Some("from") => bindings.input.core.from.clone(),
                Some("to") => bindings.input.core.to.clone(),
                Some("ruri") => bindings.input.core.ruri.clone(),
                Some(key) => bindings.input.extras.get(key).map(|v| match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                }),
                None => None,
            },
        };
        match resolved {
            Some(s) => out.push_str(&s),
            None => return Err(format!("unknown or unset binding ${{{token}}} in {value:?}")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Extract `field` from the resolved message. `Ok(None)` = legitimately absent
/// (for `exists`/`absent`); `Err` = the field selector itself is invalid here.
fn extract(
    field: &str,
    entry: &RecordedSipEntry,
    msg: &SipMessage,
) -> Result<Option<String>, String> {
    // Transport endpoints from the recorded addresses (opt-in capability).
    match field {
        "source.ip" => return Ok(Some(entry.from.ip().to_string())),
        "source.port" => return Ok(Some(entry.from.port().to_string())),
        "dest.ip" => return Ok(Some(entry.to.ip().to_string())),
        "dest.port" => return Ok(Some(entry.to.port().to_string())),
        "body" => {
            let body = match msg {
                SipMessage::Request(r) => &r.body(),
                SipMessage::Response(r) => &r.body(),
            };
            return Ok(if body.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(body).into_owned())
            });
        }
        _ => {}
    }

    // header(Name) — raw values of any header, comma-joined in wire order.
    if let Some(name) = field.strip_prefix("header(").and_then(|s| s.strip_suffix(')')) {
        let values: Vec<&str> = msg.raw(HeaderName::from(name)).collect();
        return Ok(if values.is_empty() { None } else { Some(values.join(", ")) });
    }

    // URI-bearing headers: <name>[index]?(.subfield)?
    let (head, sub) = match field.split_once('.') {
        Some((h, s)) => (h, Some(s)),
        None => (field, None),
    };
    let (name, index) = match head.split_once('[') {
        Some((n, rest)) => {
            let idx: usize = rest
                .strip_suffix(']')
                .and_then(|i| i.parse().ok())
                .ok_or_else(|| format!("bad index in field {field:?}"))?;
            (n, idx)
        }
        None => (head, 0),
    };

    // R-URI is its own shape (no display name / tag), requests only.
    if name == "ruri" {
        let SipMessage::Request(r) = msg else {
            return Err(format!("field {field:?}: the anchored message is a response"));
        };
        if index != 0 {
            return Err(format!("field {field:?}: ruri is not a list"));
        }
        let ru = r.request_uri();
        return Ok(match sub {
            None | Some("uri") => Some(ru.to_string()),
            Some("userInfo") => ru.user().map(str::to_string),
            Some("host") => Some(ru.host().to_string()),
            Some("port") => Some(ru.authority().port_or_default().to_string()),
            Some(other) => match param_selector(other)? {
                Some(p) => ru.param(p).map(param_text),
                None => return Err(format!("unknown ruri subfield {other:?} in {field:?}")),
            },
        });
    }

    let Some(field_value) = uri_header(name, index, msg)? else { return Ok(None) };
    let addr = &field_value.addr;
    let uri = addr.uri();
    Ok(match sub {
        // Bare name (or `.uri`): the URI itself — present/absent/regex over it.
        None | Some("uri") => Some(uri.to_string()),
        Some("displayName") => addr.display().map(str::to_string),
        Some("tag") => field_value.tag,
        Some("userInfo") => uri.user().map(str::to_string),
        Some("host") => Some(uri.host().to_string()),
        Some("port") => Some(uri.authority().port_or_default().to_string()),
        Some(other) => match param_selector(other)? {
            // Header param first (`;tag=`-style), then URI param. A bare flag
            // param extracts as "" (use `exists`).
            Some(p) => match addr.params().get(p) {
                Some(value) => Some(param_text(value)),
                None => uri.param(p).map(param_text),
            },
            None => return Err(format!("unknown subfield {other:?} in field {field:?}")),
        },
    })
}

/// A parameter as the check grammar reads it: its value, or `""` for a bare
/// flag (which `exists` is the op for).
fn param_text(value: &ParamValue) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// `param(x)` → `Some("x")`; anything else → `None` (unknown subfield).
fn param_selector(sub: &str) -> Result<Option<&str>, String> {
    Ok(sub.strip_prefix("param(").and_then(|s| s.strip_suffix(')')))
}

/// One address-valued header value as the check grammar sees it: the address
/// itself, plus the dialog tag — which only From and To carry.
struct AddrField {
    addr: NameAddr,
    tag: Option<String>,
}

/// The `index`-th address-shaped value of a URI-bearing header. `Ok(None)` =
/// header (or that index) absent.
fn uri_header(name: &str, index: usize, msg: &SipMessage) -> Result<Option<AddrField>, String> {
    match name {
        "from" => {
            let from = msg.from();
            let tag = from.tag().map(str::to_string);
            Ok((index == 0).then(|| AddrField { addr: from.addr().clone(), tag }))
        }
        "to" => {
            let to = msg.to();
            let tag = to.tag().map(str::to_string);
            Ok((index == 0).then(|| AddrField { addr: to.addr().clone(), tag }))
        }
        "pai" => nth(msg.list::<header::PAssertedIdentity>(), name, index),
        "ppi" => nth(msg.list::<header::PPreferredIdentity>(), name, index),
        "diversion" => nth(msg.list::<header::Diversion>(), name, index),
        // A wildcard Contact is not an address: it names every binding at once
        // (RFC 3261 §10.2.2), so it reads back as the star the UA sent.
        "contact" if msg.raw(HeaderName::Contact).any(|v| v.trim() == "*") => {
            Ok((index == 0)
                .then(|| AddrField { addr: NameAddr::new(Uri::opaque("*")), tag: None }))
        }
        "contact" => nth(msg.list::<header::Contact>(), name, index),
        other => Err(format!(
            "unknown field {other:?} (URI headers: from/to/ruri/pai/ppi/diversion/contact; \
             others via header(Name), body, source/dest)"
        )),
    }
}

fn nth<K: NameAddrKind>(
    parsed: Result<Vec<NameAddrHeader<K>>, SipParseError>,
    name: &str,
    index: usize,
) -> Result<Option<AddrField>, String> {
    match parsed {
        Ok(values) => Ok(values
            .into_iter()
            .nth(index)
            .map(|value| AddrField { addr: value.into_addr(), tag: None })),
        Err(e) => Err(format!("header {name:?} is present but malformed: {e}")),
    }
}
