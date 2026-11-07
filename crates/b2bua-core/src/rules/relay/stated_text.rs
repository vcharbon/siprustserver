//! Stated text a message the stack builds cannot carry whole (ADR-0053): a
//! header line with a name off `token` or a CR, LF or NUL in its value, a
//! single-valued header stated on more than one line (§7), a reason phrase
//! with a CR, LF or NUL. Such text is left out — the line
//! dropped, the phrase giving way to the status's default — and each skip is
//! reported ([`BufferedObservabilityEffect::StatedTextSkipped`]), so the
//! message still leaves.

use b2bua_sdk::reason_phrase::default_reason;
use sip_message::write_check::{self, WriteRefusal};
use sip_message::{FirstIngressBound, SipHeader as MsgHeader};

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
