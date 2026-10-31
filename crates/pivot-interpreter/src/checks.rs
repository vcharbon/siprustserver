//! The one **check vocabulary** (`PCAP2TEST_PIVOT_V3.md` §9), evaluated over a
//! matched message or over a deployment observable.
//!
//! `{ field, op, value }` where `op` is `eq`, `regex`, `exists` or `absent` and
//! `value` may carry `${…}` accessors. The field selector is an OPEN token: this
//! module answers the message-shaped ones it can read off `sip-message`'s
//! accessors, and refuses a selector it does not know rather than treating an
//! unreadable field as absent — "I could not read it" and "it is not there" are
//! different answers and only one of them may pass an `absent` check.

use pivot_schema::bundle::Failure;
use pivot_schema::check::{Check, CheckOp};
use sip_message::header::{HeaderValue, MediaType};
use sip_message::{header_forms_equivalent, SipStr};

use crate::gate::Inbound;
use crate::resolve::Resolver;

/// A source of observable values a check can read. The message-shaped selectors
/// are answered here; a deployment's own (a metric, a store fact) come from the
/// lane through [`Observables`].
pub trait Observables {
    /// The value of `field`, `None` where the observable does not exist, and an
    /// error where the selector is one this source does not recognize.
    fn observe(&self, field: &str) -> Result<Option<String>, String>;
}

/// The message-shaped selectors, read off a matched message.
///
/// | selector | value |
/// |---|---|
/// | `status` | the response status |
/// | `method` | the request method |
/// | `cseq` / `cseq.method` | the CSeq number / its method |
/// | `call-id` | the Call-ID |
/// | `rseq` | the RSeq of a reliable provisional |
/// | `from.tag` / `to.tag` | the dialog tags |
/// | `from.userInfo` / `to.userInfo` | the two user parts, canonically |
/// | `from.uri` / `to.uri` | the two whole URIs, parameters included |
/// | `ruri.uri` / `ruri.userInfo` | a request's Request-URI, whole / its user part canonically |
/// | `header(<Name>)` | the first occurrence of that header, in wire order |
/// | `tags(<Name>,…)` | the option tags every row of the named set-like headers lists, one set joined by `,` |
/// | `body` | the body as text |
/// | `part(<type>)` | the first entity part of that media type, as `body` reads a body |
pub struct MessageObservables<'a>(pub &'a Inbound);

impl Observables for MessageObservables<'_> {
    fn observe(&self, field: &str) -> Result<Option<String>, String> {
        let m = self.0;
        if let Some(rest) = field.strip_prefix("tags(") {
            let names = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("selector {field:?} opens `tags(` and never closes it"))?;
            let tags: Vec<String> = names
                .split(',')
                .flat_map(|name| m.headers.option_tags(name.trim()))
                .fold(Vec::new(), |mut set, tag| {
                    if !set.iter().any(|t: &String| t.eq_ignore_ascii_case(&tag)) {
                        set.push(tag);
                    }
                    set
                });
            return Ok((!tags.is_empty()).then(|| tags.join(",")));
        }
        if let Some(rest) = field.strip_prefix("part(") {
            let media = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("selector {field:?} opens `part(` and never closes it"))?;
            return Ok(part_of(m, media).map(|payload| as_text(&payload)));
        }
        if let Some(rest) = field.strip_prefix("header(") {
            let name = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("selector {field:?} opens `header(` and never closes it"))?;
            return Ok(m.header(name).map(str::to_string));
        }
        Ok(match field {
            "status" => m.status.map(|s| s.to_string()),
            "method" => m.method.clone(),
            "cseq" => Some(m.cseq.to_string()),
            "cseq.method" => Some(m.cseq_method.clone()),
            "call-id" => Some(m.call_id.clone()),
            "rseq" => m.rseq.map(|r| r.to_string()),
            "from.tag" => m.from_tag.clone(),
            "to.tag" => m.to_tag.clone(),
            "from.userInfo" => m.from_user.clone(),
            "to.userInfo" => m.to_user.clone(),
            "from.uri" => m.from_uri.clone(),
            "to.uri" => m.to_uri.clone(),
            "ruri.uri" => m.ruri.clone(),
            "ruri.userInfo" => m.ruri_user.clone(),
            // The body is bytes: observed as text when the bytes are UTF-8,
            // as standard base64 otherwise, chosen by the bytes alone.
            "body" => Some(as_text(&m.body)),
            // The byte-exact assertion an author writes for a binary body.
            "body.b64" => Some(sip_message::payload::base64(&m.body)),
            other => return Err(format!("selector {other:?} is not a message field")),
        })
    }
}

/// Bytes as a check reads them: the text they are where they are UTF-8,
/// standard base64 otherwise, chosen by the bytes alone.
fn as_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => sip_message::payload::base64(bytes),
    }
}

/// The payload of the first entity part of `m`'s body whose bare media type is
/// `media`, case-insensitively: a multipart's parts, or a single-part body as
/// its one part (RFC 2045 §5.1, RFC 2046 §5.1).
fn part_of(m: &Inbound, media: &str) -> Option<Vec<u8>> {
    let wanted = media.trim();
    sip_message::entity_parts(m.content_type.as_deref(), &[], &m.body).into_iter().find_map(
        |part| {
            let parsed = MediaType::parse(&SipStr::owned(part.content_type.as_str())).ok()?;
            parsed.token().eq_ignore_ascii_case(wanted).then_some(part.payload)
        },
    )
}

/// Evaluate `check` at `site`. The value is resolved through `resolver` first,
/// so `${leg:A.remote-tag}` compares against what THIS run minted.
pub fn evaluate(
    site: &str,
    check: &Check,
    observables: &dyn Observables,
    resolver: &Resolver<'_>,
) -> Option<Failure> {
    let observed = match observables.observe(&check.field) {
        Ok(value) => value,
        Err(detail) => {
            return Some(Failure::CheckFailed {
                site: site.to_string(),
                field: check.field.clone(),
                op: op_name(check.op).into(),
                expected: check.value.clone().unwrap_or_default(),
                observed: format!("unreadable: {detail}"),
            })
        }
    };
    let expected = match &check.value {
        None => None,
        Some(text) => match match check.op {
            CheckOp::Regex => resolver.pattern(text),
            _ => resolver.text(text),
        } {
            Ok(value) => Some(value),
            Err(e) => {
                return Some(Failure::AccessorUnresolved {
                    site: format!("{site} check {:?}", check.field),
                    detail: e.to_string(),
                })
            }
        },
    };
    let fail = |expected: String, observed: String| {
        Some(Failure::CheckFailed {
            site: site.to_string(),
            field: check.field.clone(),
            op: op_name(check.op).into(),
            expected,
            observed,
        })
    };
    match (check.op, expected) {
        (CheckOp::Exists, _) => match observed {
            Some(_) => None,
            None => fail(String::new(), "absent".into()),
        },
        (CheckOp::Absent, _) => match observed {
            None => None,
            Some(value) => fail(String::new(), value),
        },
        (CheckOp::Eq, Some(want)) => match observed {
            Some(value) if eq_holds(&check.field, &value, &want) => None,
            Some(value) => fail(want, value),
            None => fail(want, "absent".into()),
        },
        (CheckOp::Regex, Some(pattern)) => {
            let compiled = match regex::Regex::new(&pattern) {
                Ok(r) => r,
                Err(e) => return fail(pattern, format!("pattern does not compile: {e}")),
            };
            match observed {
                Some(value) if compiled.is_match(&value) => None,
                Some(value) => fail(pattern, value),
                None => fail(pattern, "absent".into()),
            }
        }
        // The plan refuses `eq`/`regex` without a value, so reaching here means
        // the check was built outside compilation; say so rather than pass.
        (op @ (CheckOp::Eq | CheckOp::Regex), None) => Some(Failure::CheckFailed {
            site: site.to_string(),
            field: check.field.clone(),
            op: op_name(op).into(),
            expected: String::new(),
            observed: "the check states no value".into(),
        }),
    }
}

/// Whether `observed` states what `want` states. On a `header(…)` selector that
/// is a comparison of wire FORMS — RFC 3261 §7.3.1 lets a list header space its
/// separators as it likes, so `Q.850; cause=16` and `Q.850;cause=16` are one
/// value — and on every other selector it is the string.
fn eq_holds(field: &str, observed: &str, want: &str) -> bool {
    match header_selector(field) {
        Some(name) => header_forms_equivalent(name, &[observed], &[want]),
        None => observed == want,
    }
}

/// The header `field` selects, where it selects one.
fn header_selector(field: &str) -> Option<&str> {
    field.strip_prefix("header(")?.strip_suffix(')')
}

fn op_name(op: CheckOp) -> &'static str {
    match op {
        CheckOp::Eq => "eq",
        CheckOp::Regex => "regex",
        CheckOp::Exists => "exists",
        CheckOp::Absent => "absent",
    }
}

#[cfg(test)]
mod tests;
