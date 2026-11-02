//! What ends a call on its liveness probe, the in-dialog OPTIONS of the
//! `keepalive` rule (RFC 3261 §11). The probe belongs to no usage of the
//! dialog (RFC 5057 §5.3): only an answer that destroys the dialog ends the
//! call — the "Destroys Dialog" responses of RFC 5057 §5.1 Table 1, and 408
//! (RFC 3261 §12.2.1.2); a 481 is `handle-481`'s — as does no answer by the
//! probe's own Timer F (RFC 3261 §12.2.1.2, §17.1.2.2). Every other refusal
//! fails the probe's transaction alone (RFC 5057 §5.1, notes 3 and 7): the
//! call stays up and the next interval probes again. A leg already going away
//! is not ended again by its probe.

use b2bua_sdk::model::{Match, RuleAction, RuleContext, RuleDefinition};
use call::{CallModelState, CdrEventType, TerminationCause, TimeoutKind};

use super::core_rules::{ok, rule};

/// The answers to the probe that destroy the dialog: RFC 5057 §5.1 Table 1
/// "Destroys Dialog", and 408 (RFC 3261 §12.2.1.2).
const DIALOG_TERMINATING: [u16; 10] = [404, 408, 410, 416, 482, 483, 484, 485, 502, 604];

/// The call ends by the probe's leg under the keepalive deadline, the CDR
/// naming the answer `status` when one came; `BeginTermination` BYEs both
/// legs (RFC 3261 §12.2.1.2: the probe may be what was lost, not the peer).
pub(super) fn keepalive_failure(
    ctx: &RuleContext,
    status: Option<u16>,
    reason: &str,
) -> Vec<RuleAction> {
    let leg_id = ctx.source_leg_id.to_string();
    vec![
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: leg_id.clone(),
            status_code: status.map(i64::from),
            reason: Some(reason.to_string()),
        },
        RuleAction::BeginTermination {
            reason: Some(status.map_or_else(|| "keepalive-timeout".into(), |s| s.to_string())),
            cause: TerminationCause::Timeout(TimeoutKind::Keepalive),
            by_leg: Some(leg_id),
        },
    ]
}

/// Whether the current response answers this stack's keepalive probe on a
/// leg that is not going away.
fn answers_the_probe(ctx: &RuleContext) -> bool {
    ctx.answered_record().is_some_and(call::RequestRecord::is_keepalive)
        && ctx
            .source_leg()
            .is_some_and(|leg| !call::helpers::leg_is_going_away(ctx.call.state(), leg))
}

/// `keepalive-dialog-terminated`: the probe drew an answer that destroys the
/// dialog (module doc).
pub(super) fn keepalive_dialog_terminated_rule() -> RuleDefinition {
    rule(
        "keepalive-dialog-terminated",
        &[],
        Match::response().method("OPTIONS").call_state(CallModelState::Active).filter(|ctx| {
            ctx.response().is_some_and(|r| DIALOG_TERMINATING.contains(&r.status()))
                && answers_the_probe(ctx)
        }),
        |ctx| {
            let Some(resp) = ctx.response() else { return ok(vec![]) };
            ok(keepalive_failure(ctx, Some(resp.status()), resp.reason()))
        },
    )
}
