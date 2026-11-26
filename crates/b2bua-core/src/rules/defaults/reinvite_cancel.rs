//! The CANCEL of a relayed re-INVITE (RFC 3261 §9): relayed onto the
//! re-INVITE toward its target, whose final answers the canceller
//! (ADR-0065), and the target's final to a re-INVITE whose originator already
//! holds this stack's final.

use call::CdrEventType;

use b2bua_sdk::model::{Match, RuleAction, RuleContext, RuleDefinition};

use super::core_rules::{ok, rule};

/// `handle-reinvite-cancel` — a CANCEL that matched a re-INVITE (the txn
/// layer answered it 200 and left the re-INVITE open; `in_dialog`: its To
/// carried a tag). The CANCEL is relayed onto the relayed re-INVITE pending
/// toward the peer, transaction-scoped (`CancelPendingReinvite`: no leg state
/// changes), the dialog and the call stay up, and the peer's final, a 487 or a
/// 200 that crossed the CANCEL, is relayed to the canceller. With no pending
/// relayed re-INVITE left to CANCEL, the turn's release ends the re-INVITE
/// with the layer's 487. Outranks `handle-cancel`, whose teardown is the
/// initial INVITE's alone.
pub(super) fn handle_reinvite_cancel() -> RuleDefinition {
    rule(
        "handle-reinvite-cancel",
        &["handle-cancel"],
        Match::cancelled().filter(|ctx| ctx.cancelled_in_dialog()),
        |ctx| {
            let Some((leg_id, outbound_cseq)) =
                ctx.cancelled_invite_cseq().and_then(|c| find_pending_relayed_invite(ctx, c))
            else {
                return ok(vec![]);
            };
            ok(vec![
                RuleAction::CancelPendingReinvite { leg_id: leg_id.clone(), outbound_cseq },
                RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Cancel,
                    leg_id,
                    status_code: None,
                    reason: Some("reinvite_cancelled".into()),
                },
            ])
        },
    )
}

/// `resolve-cancelled-reinvite-response` — the target's response to a relayed
/// re-INVITE whose originator already holds this stack's final (its snapshot
/// `cancelled`: the call's teardown answered it 487, §15.1.2, or a service
/// rejected it), CANCELled toward the target. Never relayed: a 1xx is
/// absorbed, a non-2xx drops the snapshot (§14.1: the session stays as it
/// was), and a 2xx crossing the CANCEL is ACKed and absorbed (§13.2.2.4), an
/// offer it made answered rejecting every stream. Outranks
/// `relay-reinvite-response` (whose filter skips cancelled snapshots) and the
/// INVITE-response fallbacks that would otherwise claim the final.
pub(super) fn resolve_cancelled_reinvite_response() -> RuleDefinition {
    rule(
        "resolve-cancelled-reinvite-response",
        &["relay-reinvite-response", "relay-provisional", "confirm-dialog", "route-failure"],
        Match::response()
            .method("INVITE")
            .filter(|ctx| ctx.relayed_pending().is_some_and(|p| p.cancelled)),
        |ctx| {
            let resp = ctx.response()?;
            if resp.status() < 200 {
                return ok(vec![]);
            }
            let leg_id = ctx.source_leg_id.to_string();
            let outbound_cseq = resp.cseq().seq() as i64;
            ok(vec![RuleAction::ResolveCancelledReinvite { leg_id, outbound_cseq }])
        },
    )
}

/// The leg carrying the still-pending relayed re-INVITE a CANCEL targets: the
/// pending-relay snapshot whose `inbound_cseq` equals the CANCELled INVITE's
/// CSeq (the canceller's own CSeq space), with the snapshot's `outbound_cseq`
/// (the relayed transaction's CSeq on that leg's dialog). At most one relayed
/// INVITE is in flight per call (`reinvite-glare` 491s a second), so the first
/// match is the match; a snapshot already `cancelled`, or whose CANCEL was
/// already relayed, is skipped (a retransmitted CANCEL must not re-CANCEL).
fn find_pending_relayed_invite(ctx: &RuleContext, inbound_cseq: i64) -> Option<(String, i64)> {
    std::iter::once(ctx.call.a_leg()).chain(ctx.call.b_legs().iter()).find_map(|leg| {
        leg.dialogs.iter().find_map(|d| {
            call::helpers::invite_relay(d)
                .filter(|p| p.inbound_cseq == inbound_cseq && !p.cancelled && !p.cancel_relayed)
                .map(|p| (leg.leg_id.clone(), p.outbound_cseq))
        })
    })
}
