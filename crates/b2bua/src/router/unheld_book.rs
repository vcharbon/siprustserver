//! The one book that bounds every answer to an INVITE final no call here
//! holds (ADR-0063 §2): a vanished call's final, a held call's final the
//! dialog gate drops, a held call's non-2xx no transaction matched, and a
//! 2xx naming a leg the record lacks ([`b2bua_core::unheld_response`]). The
//! router consults it for its own answers and the commit for the core's.
//!
//! Keyed by the final's Call-ID, To-tag and CSeq ([`UnheldKey`]). Its first
//! arrival draws the whole answer: the ACK, and for a 2xx the BYE, which
//! leaves once. A later copy draws the ACK alone while it is owed one
//! ([`b2bua_core::copy_schedule::owed`]) and within 64·T1 of the first
//! arrival, past which a compliant answerer stopped re-sending; any other copy
//! is absorbed and counted (`b2bua_copies_past_schedule_total{kind="unheld"}`).
//! An entry lives until 64·T1 after its last copy, absorbed copies included.
//! The book is node-local: a takeover node answers from its own.

use std::collections::HashMap;
use std::sync::Mutex;

use b2bua_core::copy_schedule;
use b2bua_core::effects::{HandlerEffects, OutboundSipEffect};
use b2bua_core::unheld_response::{UnheldAnswer, UnheldKey};
use sip_retransmit::timers::T1;

use super::RouterCtx;

/// How long after a final's first arrival a copy is still answered, and how
/// long after its last copy its entry is kept: 64·T1, the bound a compliant
/// answerer's ladder ends within (RFC 3261 §13.3.1.4, §17.2.1).
#[expect(clippy::cast_possible_wrap, reason = "a const: 64·T1 fits an i64")]
const WINDOW_MS: i64 = 64 * T1 as i64;

/// The `kind` label the book's absorbed copies count under.
const KIND: &str = "unheld";

/// One final's answers so far.
struct Entry {
    first_ms: i64,
    /// The copies counted after the first arrival.
    copies: u32,
    answered_ms: i64,
    last_ms: i64,
}

impl Entry {
    fn live(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_ms) < WINDOW_MS
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<UnheldKey, Entry>,
    pruned_ms: i64,
}

impl Inner {
    /// Drop the entries past their window, at most once per T1.
    fn prune(&mut self, now_ms: i64) {
        if u64::try_from(now_ms.saturating_sub(self.pruned_ms)).is_ok_and(|since| since < T1) {
            return;
        }
        self.pruned_ms = now_ms;
        self.entries.retain(|_, e| e.live(now_ms));
    }
}

/// The answers to unheld finals this node sent, by final.
#[derive(Default)]
pub(crate) struct UnheldBook {
    inner: Mutex<Inner>,
}

impl UnheldBook {
    /// What `answer`, drawn by a copy of its final arriving at `now_ms`,
    /// sends: everything on the final's first arrival, the ACK alone on a
    /// copy owed one, `None` for a copy absorbed.
    fn admit(&self, answer: UnheldAnswer, now_ms: i64) -> Option<Vec<OutboundSipEffect>> {
        let mut inner = crate::poison::locked(&self.inner, crate::poison::UNHELD_ANSWERS);
        inner.prune(now_ms);
        let entry = inner.entries.get_mut(&answer.key).filter(|e| e.live(now_ms));
        let Some(entry) = entry else {
            let first = Entry { first_ms: now_ms, copies: 0, answered_ms: now_ms, last_ms: now_ms };
            inner.entries.insert(answer.key.clone(), first);
            return Some(answer.into_effects());
        };
        entry.copies = entry.copies.saturating_add(1);
        entry.last_ms = now_ms;
        let since = now_ms.saturating_sub(entry.answered_ms);
        let owed = now_ms.saturating_sub(entry.first_ms) < WINDOW_MS
            && copy_schedule::owed(answer.class, entry.copies, Some(since));
        if !owed {
            return None;
        }
        entry.answered_ms = now_ms;
        Some(vec![answer.ack])
    }
}

/// What `answer`, drawn at `now_ms`, sends through the node's book; an
/// absorbed copy sends nothing and is counted.
pub(super) fn admitted(
    ctx: &RouterCtx,
    answer: UnheldAnswer,
    now_ms: i64,
) -> Vec<OutboundSipEffect> {
    ctx.unheld_answers.admit(answer, now_ms).unwrap_or_else(|| {
        ctx.metrics.record_copy_past_schedule(KIND);
        Vec::new()
    })
}

/// Move what the turn's unheld answers send, drawn at `now_ms`, onto its
/// outbound effects, behind the turn's own.
pub(super) fn admit_turn_answers(ctx: &RouterCtx, effects: &mut HandlerEffects, now_ms: i64) {
    for answer in std::mem::take(&mut effects.unheld) {
        effects.outbound.extend(admitted(ctx, answer, now_ms));
    }
}

#[cfg(test)]
#[path = "unheld_book_tests.rs"]
mod tests;
