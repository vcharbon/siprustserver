//! Stated text a message the stack builds cannot carry whole (ADR-0053): a
//! header line with a name off `token` or a CR, LF or NUL in its value, a
//! reason phrase with a CR, LF or NUL. Such text is left out — the line
//! dropped, the phrase giving way to the status's default — and each skip is
//! reported ([`BufferedObservabilityEffect::StatedTextSkipped`]), so the
//! message still leaves.

use b2bua_sdk::reason_phrase::default_reason;
use sip_message::write_check::{self, WriteRefusal};
use sip_message::SipHeader as MsgHeader;

use crate::effects::{BufferedObservabilityEffect, HandlerEffects};

/// Whether the wire carries `line` whole; a refusal is pushed onto `skipped`.
pub(crate) fn writable(line: &MsgHeader, skipped: &mut Vec<WriteRefusal>) -> bool {
    match write_check::header_line(&line.name, &line.value) {
        Ok(()) => true,
        Err(refusal) => {
            skipped.push(refusal);
            false
        }
    }
}

/// `lines` without those the wire cannot carry whole, each one left out
/// reported on `fx` under `what`.
pub(crate) fn writable_lines(
    what: &str,
    lines: Vec<MsgHeader>,
    fx: &mut HandlerEffects,
) -> Vec<MsgHeader> {
    let mut skipped = Vec::new();
    let kept = lines.into_iter().filter(|line| writable(line, &mut skipped)).collect();
    fx.buffered.extend(BufferedObservabilityEffect::skipped(what, skipped));
    kept
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
