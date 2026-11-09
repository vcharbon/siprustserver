//! Stated text a message the stack builds cannot carry whole (ADR-0053): a
//! header line with a name off `token` or a CR, LF or NUL in its value, a
//! single-valued header stated on more than one line (§7), a reason phrase
//! with a CR, LF or NUL. Such text is left out — the line
//! dropped, the phrase giving way to the status's default — and each skip is
//! reported ([`BufferedObservabilityEffect::StatedTextSkipped`]), so the
//! message still leaves.

use b2bua_sdk::reason_phrase::default_reason;
use sip_message::header::{HeaderClass, HeaderName};
use sip_message::write_check::{self, WriteRefusal};
use sip_message::{FirstIngressBound, SipHeader as MsgHeader, SipStr};

use crate::effects::{BufferedObservabilityEffect, HandlerEffects};

/// Whether the wire carries `line` whole; a refusal is pushed onto `skipped`.
pub(crate) fn writable(line: &MsgHeader, skipped: &mut Vec<WriteRefusal>) -> bool {
    kept(write_check::header_line(&line.name, &line.value), skipped)
}

/// [`writable`] on a new request, which its next hop reads at first ingress
/// under `bound`.
pub(crate) fn writable_on_new_request(
    line: &MsgHeader,
    bound: &FirstIngressBound,
    skipped: &mut Vec<WriteRefusal>,
) -> bool {
    kept(write_check::new_request_line(&line.name, &line.value, bound), skipped)
}

fn kept(checked: Result<(), WriteRefusal>, skipped: &mut Vec<WriteRefusal>) -> bool {
    match checked {
        Ok(()) => true,
        Err(refusal) => {
            skipped.push(refusal);
            false
        }
    }
}

/// `lines` without those the wire cannot carry whole and without every line
/// of a single-valued header they state more than once
/// ([`write_check::one_line_each`]), each one left out reported on `fx` under
/// `what`.
pub(crate) fn writable_lines(
    what: &str,
    lines: Vec<MsgHeader>,
    fx: &mut HandlerEffects,
) -> Vec<MsgHeader> {
    let mut skipped = Vec::new();
    let lines = write_check::one_line_each(lines, &mut skipped);
    let kept = lines.into_iter().filter(|line| writable(line, &mut skipped)).collect();
    fx.buffered.extend(BufferedObservabilityEffect::skipped(what, skipped));
    kept
}

/// The non-structural `header_updates` *sets*. Removals and structural keys
/// drop: the response generator owns the stack-owned set (ADR-0017 X2).
pub(crate) fn set_lines(header_updates: &[(String, Option<String>)]) -> Vec<MsgHeader> {
    header_updates
        .iter()
        .filter(|(name, _)| HeaderName::class_of(name) == HeaderClass::EndToEnd)
        .filter_map(|(name, val)| {
            let value = val.as_deref()?;
            Some(MsgHeader { name: SipStr::owned(name), value: SipStr::owned(value) })
        })
        .collect()
}

/// What a service's `header_updates` state on a response this stack authors:
/// the lines it sets ([`set_lines`]) less every line of a single-valued header
/// set more than once, which is as though never stated (ADR-0053 §7).
pub(crate) struct Statements<'a> {
    updates: &'a [(String, Option<String>)],
    pub(crate) lines: Vec<MsgHeader>,
}

impl<'a> Statements<'a> {
    /// The statements of `updates`, a skip reported on `fx` under `what`.
    pub(crate) fn of(
        what: &str,
        updates: &'a [(String, Option<String>)],
        fx: &mut HandlerEffects,
    ) -> Self {
        let mut skipped = Vec::new();
        let lines = write_check::one_line_each(set_lines(updates), &mut skipped);
        fx.buffered.extend(BufferedObservabilityEffect::skipped(what, skipped));
        Self { updates, lines }
    }

    /// Whether the header `name` names is the service's: a line kept, a
    /// removal, or a name the generator owns.
    pub(crate) fn states(&self, name: &str) -> bool {
        let named = HeaderName::from(name);
        let unwritten = |n: &str, v: &Option<String>| {
            v.is_none() || HeaderName::class_of(n) != HeaderClass::EndToEnd
        };
        self.lines.iter().any(|h| named.matches(&h.name))
            || self.updates.iter().any(|(n, v)| unwritten(n, v) && named.matches(n))
    }
}

/// `status` and `reason` when a message can carry that status, else this stack's
/// internal error, the rule's defect reported on `fx` under `what`: the party is
/// answered, not left to its timer.
pub(crate) fn writable_status<'a>(
    what: &str,
    status: u16,
    reason: &'a str,
    fx: &mut HandlerEffects,
) -> (u16, &'a str) {
    if sip_message::is_status_code(status) {
        return (status, reason);
    }
    fx.buffered.push(BufferedObservabilityEffect::StatedTextSkipped {
        detail: format!("{what}: {status} is no status code"),
    });
    (500, default_reason(500))
}

/// `reason` when the wire carries it, else the default phrase of `status`,
/// the refusal reported on `fx` under `what`.
pub(crate) fn writable_reason(
    what: &str,
    status: u16,
    reason: &str,
    fx: &mut HandlerEffects,
) -> String {
    match write_check::reason_phrase(reason) {
        Ok(()) => reason.to_string(),
        Err(refusal) => {
            fx.buffered.extend(BufferedObservabilityEffect::skipped(what, vec![refusal]));
            default_reason(status).to_string()
        }
    }
}
