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
use sip_message::header_forms_equivalent;

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
/// | `header(<Name>)` | the first occurrence of that header, in wire order |
/// | `body` | the body as text |
pub struct MessageObservables<'a>(pub &'a Inbound);

impl Observables for MessageObservables<'_> {
    fn observe(&self, field: &str) -> Result<Option<String>, String> {
        let m = self.0;
        if let Some(rest) = field.strip_prefix("header(") {
            let name = rest.strip_suffix(')').ok_or_else(|| {
                format!("selector {field:?} opens `header(` and never closes it")
            })?;
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
            "body" => Some(String::from_utf8_lossy(&m.body).into_owned()),
            other => return Err(format!("selector {other:?} is not a message field")),
        })
    }
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
        Some(text) => match resolver.text(text) {
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
mod tests {
    use super::*;
    use pivot_schema::bundle::IdentityBindings;
    use crate::state::RunState;

    fn inbound() -> Inbound {
        Inbound {
            method: None,
            status: Some(200),
            reason: Some("OK".into()),
            cseq_method: "INVITE".into(),
            cseq: 1,
            call_id: "call-a@h".into(),
            rseq: None,
            headers: vec![("To".into(), "<sip:bob@h>;tag=t7".into())].into(),
            body: vec![],
            content_type: None,
            ruri_user: None,
            from_tag: Some("a1".into()),
            to_tag: Some("t7".into()),
            from_user: Some("0009001".into()),
            to_user: Some("bob".into()),
        }
    }

    fn check(field: &str, op: CheckOp, value: Option<&str>) -> Check {
        Check { field: field.into(), op, value: value.map(str::to_string), class: None }
    }

    fn run<'a>(
        check: &Check,
        inbound: &Inbound,
        state: &'a RunState,
        bindings: &'a IdentityBindings,
    ) -> Option<Failure> {
        let resolver = Resolver::new(state, bindings);
        evaluate("s1", check, &MessageObservables(inbound), &resolver)
    }

    #[test]
    fn every_op_reads_the_message_and_says_what_it_saw() {
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let m = inbound();
        assert!(run(&check("status", CheckOp::Eq, Some("200")), &m, &state, &bindings).is_none());
        let failed = run(&check("status", CheckOp::Eq, Some("486")), &m, &state, &bindings);
        assert!(
            matches!(&failed, Some(Failure::CheckFailed { expected, observed, .. })
                if expected == "486" && observed == "200"),
            "{failed:?}"
        );
        assert!(run(&check("header(To)", CheckOp::Exists, None), &m, &state, &bindings).is_none());
        assert!(run(&check("header(Replaces)", CheckOp::Absent, None), &m, &state, &bindings).is_none());
        assert!(run(&check("header(To)", CheckOp::Absent, None), &m, &state, &bindings).is_some());
        assert!(run(&check("header(To)", CheckOp::Regex, Some("tag=t\\d+")), &m, &state, &bindings).is_none());
        assert!(run(&check("header(To)", CheckOp::Regex, Some("^nope$")), &m, &state, &bindings).is_some());
    }

    #[test]
    fn a_check_value_resolves_through_the_run_s_own_state() {
        let mut state = RunState::new();
        state.leg_mut("A").remote_tag = Some("t7".into());
        let bindings = IdentityBindings::new();
        let matched = check("header(To)", CheckOp::Regex, Some("tag=${leg:A.remote-tag}$"));
        assert!(run(&matched, &inbound(), &state, &bindings).is_none());
        let unresolvable = check("status", CheckOp::Eq, Some("${leg:Z.remote-tag}"));
        assert!(matches!(
            run(&unresolvable, &inbound(), &state, &bindings),
            Some(Failure::AccessorUnresolved { .. })
        ));
    }

    #[test]
    fn the_dialog_selectors_the_spec_names_read_off_the_message() {
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let m = inbound();
        for (field, value) in [
            ("from.tag", "a1"),
            ("to.tag", "t7"),
            ("from.userInfo", "0009001"),
            ("to.userInfo", "bob"),
        ] {
            assert!(
                run(&check(field, CheckOp::Eq, Some(value)), &m, &state, &bindings).is_none(),
                "{field}"
            );
            assert!(run(&check(field, CheckOp::Exists, None), &m, &state, &bindings).is_none());
        }
        // The spec's own example: a tag compared against what the run minted.
        let mut state = RunState::new();
        state.leg_mut("A").remote_tag = Some("t7".into());
        let pinned = check("to.tag", CheckOp::Eq, Some("${leg:A.remote-tag}"));
        assert!(run(&pinned, &m, &state, &bindings).is_none());
        // A tag the message does not carry is ABSENT, and `eq` says so.
        let mut untagged = inbound();
        untagged.to_tag = None;
        assert!(run(&check("to.tag", CheckOp::Absent, None), &untagged, &state, &bindings).is_none());
        let failed = run(&check("to.tag", CheckOp::Eq, Some("t7")), &untagged, &state, &bindings);
        assert!(
            matches!(&failed, Some(Failure::CheckFailed { observed, .. }) if observed == "absent"),
            "{failed:?}"
        );
    }

    /// **Issue 68**: `eq` on a header is a comparison of wire FORMS. Whitespace
    /// around a list separator is layout (RFC 3261 §7.3.1 / §25.1); a value
    /// difference is still a difference, and a non-header selector is untouched.
    #[test]
    fn eq_on_a_header_reads_its_form_and_on_anything_else_its_bytes() {
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let mut m = inbound();
        m.headers = vec![
            ("Reason".into(), "Q.850; cause=16".into()),
            ("Allow".into(), "INVITE, ACK, BYE".into()),
        ]
        .into();
        let eq = |field: &str, value: &str| check(field, CheckOp::Eq, Some(value));
        assert!(run(&eq("header(Reason)", "Q.850;cause=16"), &m, &state, &bindings).is_none());
        assert!(run(&eq("header(Allow)", "INVITE,ACK,BYE"), &m, &state, &bindings).is_none());
        assert!(run(&eq("header(Reason)", "Q.850;cause=127"), &m, &state, &bindings).is_some());
        assert!(run(&eq("header(Allow)", "INVITE,ACK"), &m, &state, &bindings).is_some());
        // A status is not a header list: it compares as the string it is.
        assert!(run(&eq("status", " 200"), &m, &state, &bindings).is_some());
    }

    #[test]
    fn a_selector_the_source_cannot_read_fails_rather_than_reading_as_absent() {
        let state = RunState::new();
        let bindings = IdentityBindings::new();
        let unreadable = check("sip_transactions_orphaned_total", CheckOp::Absent, None);
        let failure = run(&unreadable, &inbound(), &state, &bindings);
        assert!(
            matches!(&failure, Some(Failure::CheckFailed { observed, .. }) if observed.starts_with("unreadable")),
            "{failure:?}"
        );
    }
}
