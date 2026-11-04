//! The ACK a peer's BYE draws before its answer: a rule that answers a peer's
//! BYE by ending its leg ([`LegEnd::PeerByeAnswered`]) first ACKs the 2xx that
//! peer answered this stack's INVITE with, where the ACK owes no answer
//! (RFC 3261 §13.2.2.4) — the peer's INVITE server transaction completes
//! before its BYE's does.

use b2bua_sdk::model::{LegEnd, RuleAction, RuleContext};
use call::Call;
use sip_message::Method;

use crate::effects::HandlerEffects;
use crate::rules::relay;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// ACK, ahead of `actions`, the 2xx the sender of the turn's BYE answered
    /// when `actions` end its leg on that BYE and the ACK owes no answer.
    pub(super) fn ack_before_peer_bye_answer(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        actions: &[RuleAction],
    ) {
        let source = ctx.source_leg_id;
        if !ctx.request().is_some_and(|r| *r.method() == Method::Bye) {
            return;
        }
        let ends_source = actions.iter().any(|a| {
            matches!(a, RuleAction::TerminateLeg { leg_id, end: LegEnd::PeerByeAnswered }
                if leg_id == source)
        });
        let owed = call::helpers::find_leg(call, source).is_some_and(relay::bare_ack_owed);
        if ends_source && owed {
            self.ack_own_bare(call, fx, source);
        }
    }
}
