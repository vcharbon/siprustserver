//! Fork stragglers: a 2xx from a fork other than the one a 2xx established
//! the b-leg with (RFC 3261 §13.2.2.4), and the BYE this stack releases it
//! with, whose final or timeout ends the release alone.

use b2bua_sdk::model::RuleContext;

/// Whether the turn's timeout is the BYE that released a fork straggler on
/// the source leg.
pub(super) fn straggler_release_timeout(ctx: &RuleContext) -> bool {
    ctx.timed_out_record().is_some() && straggler_release(ctx.resolved)
}

/// Whether the record a final or a timeout settled is the BYE that released a
/// fork straggler: this stack's BYE a retired dialog holds (RFC 3261
/// §13.2.2.4). Its end is the release's alone.
pub(super) fn straggler_release(resolved: &call::helpers::Resolved) -> bool {
    resolved.record.as_deref().is_some_and(|(holder, r)| {
        matches!(holder, call::helpers::RecordHolder::Retired(_))
            && r.is_own()
            && r.method.eq_ignore_ascii_case("BYE")
    })
}

/// Whether the current response is a fork straggler's 2xx on its b-leg: a
/// To-tag naming no dialog of the leg, or a retired one, on a leg a 2xx
/// established, answering an INVITE no later than the one the established
/// dialog's ACK acknowledges (every fork of one INVITE answers on its CSeq,
/// RFC 3261 §12.1.2).
pub(super) fn fork_straggler(ctx: &RuleContext) -> bool {
    let Some(resp) = ctx.response() else { return false };
    if resp.to().tag().is_none_or(str::is_empty) {
        return false;
    }
    let Some(leg) = ctx.call.b_legs().iter().find(|l| l.leg_id == ctx.source_leg_id) else {
        return false;
    };
    let Some(established) = leg.dialogs.established() else { return false };
    ctx.dialog().is_none_or(|d| d.retired().is_some())
        && crate::rules::relay::acked_invite_cseq(leg, established)
            .is_some_and(|acked| resp.cseq().seq() <= acked)
}
