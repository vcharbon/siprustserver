//! What a client transaction's timeout means for `handle-timeout`, before its
//! INVITE readings: the request record the timeout settled, read by its origin
//! and the method table (ADR-0055 §2); a leg already going away; the BYE that
//! released a fork straggler.

use b2bua_sdk::model::{LegEnd, RuleAction, RuleContext};
use call::{ByeDisposition, LegDisposition, TerminationCause, TimeoutKind};

use crate::rules::method_table::OwnTimeout;

/// What the timeout of the request `record` settled means (ADR-0055 §2). A
/// relay's is the relayed exchange's: the originator is sent no 408 (RFC 4320
/// §4.2) and the session stands (RFC 5057 §5.2). This stack's own request's
/// reads the method table: a PRACK's denies that acknowledgement alone; on a
/// leg going away it is the leg's end; a NOTIFY's ends its subscription
/// alone, which a service owning it reads; any other ends the session.
pub(super) fn record_timeout_actions(
    ctx: &RuleContext,
    record: &call::RequestRecord,
) -> Vec<RuleAction> {
    let own = crate::rules::method_table::row_of(&record.method).own_timeout;
    if record.is_relay() || own == OwnTimeout::Transaction {
        return vec![];
    }
    if let Some(leg) = ctx.source_leg() {
        if call::helpers::leg_is_going_away(ctx.call.state(), leg) {
            return going_away_timeout(ctx, leg);
        }
    }
    match own {
        OwnTimeout::Transaction | OwnTimeout::Usage => vec![],
        OwnTimeout::Session => vec![RuleAction::BeginTermination {
            reason: Some("timeout".into()),
            cause: TerminationCause::Timeout(TimeoutKind::Transaction),
            by_leg: Some(ctx.source_leg_id.to_string()),
        }],
    }
}

/// A transaction timing out on `leg`, a leg already going away, makes no
/// forward progress: what the leg was awaiting names its end. A leg awaiting
/// its BYE's answer is resolved by that BYE's own timeout only: an earlier
/// request's dead transaction (the liveness OPTIONS that opened the teardown,
/// Timer F still running) says nothing about the BYE, which may yet be
/// answered. A leg already ended (terminal disposition, no CANCEL in flight)
/// keeps its disposition when a request sent after its dialog ended times out
/// (a NOTIFY on a surviving subscription usage, RFC 5057 §5.2); the requests
/// it still holds are settled by the leg's re-termination. Anything else is
/// abandoned.
pub(super) fn going_away_timeout(ctx: &RuleContext, leg: &call::Leg) -> Vec<RuleAction> {
    let leg_id = ctx.source_leg_id.to_string();
    if leg.disposition() != LegDisposition::Cancelling
        && matches!(leg.end(), Some(b) if b.is_terminal())
    {
        return vec![RuleAction::TerminateLeg { leg_id, end: LegEnd::Settle }];
    }
    let timed_out_bye = ctx.timeout_method().is_some_and(|m| m.eq_ignore_ascii_case("BYE"));
    let end = match leg.end() {
        Some(ByeDisposition::ByeSent) if timed_out_bye => LegEnd::OurByeTimedOut,
        Some(ByeDisposition::ByeSent) => return vec![],
        _ => LegEnd::Abandon,
    };
    vec![RuleAction::TerminateLeg { leg_id, end }]
}

/// Whether the turn's timeout is the BYE that released a fork straggler on
/// the source leg ([`call::StragglerRelease`]).
pub(super) fn straggler_release_timeout(ctx: &RuleContext) -> bool {
    let branch = ctx.timeout_branch().unwrap_or_default();
    ctx.source_leg().is_some_and(|l| {
        l.dialogs
            .iter()
            .filter_map(|d| d.retired())
            .any(|r| r.release.as_ref().is_some_and(|rel| rel.bye_branch == branch))
    })
}
