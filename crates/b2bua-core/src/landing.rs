//! The landing pass a turn's result takes before the shell persists it
//! (ADR-0052 §6): the answer deadlines of the requests the turn sent off its
//! turn, the refresh arming, the stated headers on every outbound message, the
//! non-INVITE finals it sends, the message ring's record of what the turn
//! sends and the turn's seal.

use std::time::Duration;

use call::helpers::{seal_termination_seq, seal_turn};

use crate::answer_deadline::AnswerBudgets;
use crate::config::B2buaConfig;
use crate::effects::HandlerResult;

/// The values the landing pass reads, which the shell computes from its
/// handles.
pub struct Landing<'a> {
    pub config: &'a B2buaConfig,
    /// The budgets the requests sent off a call's turn are answered by.
    pub budgets: AnswerBudgets,
    /// The limiter's refresh period.
    pub refresh_period: Duration,
}

/// `result` as it lands at `now_ms`.
pub fn land(result: HandlerResult, landing: &Landing<'_>, now_ms: i64) -> HandlerResult {
    // Every request sent off the call's turn has its answer's deadline on
    // the record the turn lands (ADR-0039).
    let result = crate::answer_deadline::arm(result, &landing.budgets, now_ms);
    // A counted live call always has its refresh armed within one refresh
    // period of the learnt lease, before the record lands: the one arming of
    // the refresh, for the admitting turn, the refresh turn and every other.
    let result = crate::limiter::call::arm_refresh(result, now_ms, landing.refresh_period);
    // Every message the turn sends takes the call's stated headers before the
    // ring records it or the wire sees it.
    let result = crate::rules::stated_headers::stamp_outbound(result);
    // Every non-INVITE final the turn sends is on the record as it leaves, so
    // a copy of its request draws it again wherever the call is served.
    let result = crate::answered_finals::retain(result, now_ms);
    // What the turn sends is on the record before the record lands, and a
    // termination this turn began is cut after it: every ring entry with
    // `seq <= termination.last_seq` was received or sent as part of
    // beginning the termination, every later one came after (the peer's 200
    // to the relayed BYE, the ACK to a 487). The turn is sealed with it, so
    // the next turn's entries take the next number. With the ring off the
    // cut stays `0`.
    match crate::message_ring::Ring::of(landing.config) {
        Some(ring) => HandlerResult {
            call: {
                let mut call = ring.sent(result.call, &result.effects.outbound, now_ms);
                seal_termination_seq(&mut call);
                seal_turn(&mut call);
                call
            },
            effects: result.effects,
        },
        None => result,
    }
}
