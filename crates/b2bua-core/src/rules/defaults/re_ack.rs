//! The re-ACK of a copy of a 2xx this stack's INVITE drew (RFC 3261
//! §13.2.2.4): one ACK per 2xx, re-passed to the transport as it left for
//! every copy the answerer sends until it sees it (§13.3.1.4), under the bound
//! ADR-0057 §10 sets.

use call::LegState;

use b2bua_sdk::model::{Match, RuleAction, RuleDefinition};

use super::core_rules::{ok, rule};

/// `re-ack-retransmitted-2xx` — re-ACK a **retransmitted 2xx** whose first ACK
/// was lost. §13.2.2.4 gives one ACK per 2xx and re-passes THAT ACK to the
/// transport for every copy, so the re-ACK reuses the client transaction the
/// ACK went out on (the retained `ack_branch` + the INVITE CSeq): a fresh
/// branch would mint a new transaction and never quiesce the answerer's INVITE
/// server txn. `ack_leg` re-passes the retained datagram (`emitted_ack`) raw,
/// so whatever body the acknowledging party put on it rides every repeat. This
/// is the b-leg / realign twin of the a-leg `unacked-2xx-retransmit` (which
/// retransmits the B2BUA's *own* 2xx to a silent caller).
///
/// Fires ONLY on a genuine retransmit that nothing else claims: the source
/// dialog holds an `ack_branch` — its ACK client transaction, minted by the
/// ACK's emission and reset on every new INVITE txn — the response echoes that
/// INVITE's CSeq, and no pending-relay snapshot is open (a first-time
/// re-INVITE final is claimed by `relay-reinvite-response` / the realign-200
/// rules, whose `ack_branch` is still `None`). A 2xx whose ACK has not left yet
/// draws nothing: the answerer's own ladder is the mechanism §13.3.1.4
/// provides while that ACK is on its way. Absorbs (AckLeg only — no relay to
/// the peer, which already saw the first final).
///
/// A leg the BYE already Terminated still owes it (RFC 5407 §2 + App. D: a
/// Mortal UA keeps the invite usage for exactly this, and §13.2.2.4's "every
/// time a retransmission arrives" does not lapse with the dialog).
pub(super) fn re_ack_retransmitted_2xx() -> RuleDefinition {
    rule(
        "re-ack-retransmitted-2xx",
        &[],
        Match::response()
            .method("INVITE")
            .status_class(2)
            .leg_states(&[LegState::Confirmed, LegState::Terminated])
            .filter(|ctx| {
                let Some(d) = ctx.dialog() else { return false };
                let Some(leg) = ctx.source_leg() else { return false };
                let Some(resp) = ctx.response() else { return false };
                call::helpers::sent_exchange(d).is_some_and(|x| x.ack_branch.is_some())
                    && crate::rules::relay::retransmitted_2xx(leg, d, resp)
            }),
        |ctx| ok(crate::rules::relay::re_ack(ctx)),
    )
}

/// `re-ack-2xx-ahead` — a copy of the 2xx to the INVITE before the dialog's
/// last (§13.3.1.4), arriving once that last INVITE left, answers nothing
/// pending (§12.2.1.2): it is re-ACKed with the ACK that 2xx drew, which the
/// last INVITE's record carries ahead, on that 2xx's own count (ADR-0057 §10).
/// A Terminated leg still owes it, as above.
pub(super) fn re_ack_2xx_ahead() -> RuleDefinition {
    rule(
        "re-ack-2xx-ahead",
        &[],
        Match::response()
            .method("INVITE")
            .status_class(2)
            .leg_states(&[LegState::Confirmed, LegState::Terminated])
            .filter(|ctx| {
                let (Some(d), Some(resp)) = (ctx.dialog(), ctx.response()) else { return false };
                crate::rules::relay::copy_of_earlier_2xx(d, resp)
                    && call::helpers::sent_exchange(d)
                        .is_some_and(|x| crate::rules::relay::copy_of_2xx_ahead(x, resp))
            }),
        |ctx| ok(vec![RuleAction::ReAckAhead { leg_id: ctx.source_leg_id.to_string() }]),
    )
}
