//! The finding a rule raises against the sender of the message it judges.

use crate::verdict::{Decision, Finding, RuleId};
use crate::wire::Msg;

/// A finding on the message at `mi`, charging the endpoint that emitted it and
/// read at the endpoint that took it.
pub(super) fn charge(rule: RuleId, msg: &Msg, mi: usize, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: msg.src.clone(),
        taker: msg.dst.clone(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}
