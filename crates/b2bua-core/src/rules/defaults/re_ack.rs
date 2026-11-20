//! The re-ACK of a copy of a 2xx this stack's INVITE drew (RFC 3261
//! §13.2.2.4): one ACK per 2xx, re-passed to the transport as it left for
//! every copy the answerer sends until it sees it (§13.3.1.4), under the bound
//! ADR-0057 §10 sets.

use call::LegState;

use b2bua_sdk::model::{Match, RuleAction, RuleDefinition};

use super::core_rules::{ok, rule};

/// `re-ack-retransmitted-2xx` — a copy of the 2xx to the dialog's last INVITE,
/// once its ACK left (the dialog holds its `ack_branch`), on a Confirmed or
/// Terminated leg: the retained ACK datagram is re-passed raw, nothing is
/// relayed. Rationale: ADR-0057 appendix §10.
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
