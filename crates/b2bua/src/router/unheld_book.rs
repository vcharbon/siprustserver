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
//!
//! A turn's admission is taken back unless its answers reach the call's
//! outbox ([`Admission`]): a turn dropped or fenced off before, its BYE
//! never left, leaves the book as it found it.
//!
//! The book holds at most [`CAPACITY`] finals; past it, the half idle longest
//! is forgotten, and a later copy of one of those draws its whole answer again
//! (a second BYE into an ended dialog draws a 481, RFC 3261 §15.1.2).

use std::collections::HashMap;
use std::sync::Mutex;

use b2bua_core::copy_schedule;
use b2bua_core::effects::{HandlerResult, OutboundSipEffect};
use b2bua_core::unheld_response::{UnheldAnswer, UnheldKey};
use sip_retransmit::timers::T1;

use super::RouterCtx;

/// How long after a final's first arrival a copy is still answered, and how
/// long after its last copy its entry is kept: 64·T1, the bound a compliant
/// answerer's ladder ends within (RFC 3261 §13.3.1.4, §17.2.1).
#[expect(clippy::cast_possible_wrap, reason = "a const: 64·T1 fits an i64")]
const WINDOW_MS: i64 = 64 * T1 as i64;

/// The most finals the book holds.
pub(crate) const CAPACITY: usize = 1 << 16;

/// The `kind` label the book's absorbed copies count under.
const KIND: &str = "unheld";

/// One final's answers so far.
#[derive(Clone, Copy)]
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

/// What one admission changed: each key it touched, with its entry before.
type Undo = Vec<(UnheldKey, Option<Entry>)>;

struct Inner {
    entries: HashMap<UnheldKey, Entry>,
    pruned_ms: i64,
    capacity: usize,
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

    /// Make room for one more final: at capacity, keep at most the half
    /// copied most recently, none copied at the instant of the latest one
    /// forgotten.
    fn make_room(&mut self) {
        if self.entries.len() < self.capacity {
            return;
        }
        let mut last: Vec<i64> = self.entries.values().map(|e| e.last_ms).collect();
        let forgotten = self.entries.len() - self.capacity / 2;
        let (_, &mut cutoff, _) = last.select_nth_unstable(forgotten - 1);
        self.entries.retain(|_, e| e.last_ms > cutoff);
        tracing::warn!(
            capacity = self.capacity,
            kept = self.entries.len(),
            "the book of unheld answers is full: the finals idle longest are forgotten"
        );
    }

    /// What `answer`, drawn by a copy of its final arriving at `now_ms`,
    /// sends, its key's entry before noted in `undo`: everything on the
    /// final's first arrival, the ACK alone on a copy owed one, `None` for a
    /// copy absorbed.
    fn admit(
        &mut self,
        answer: UnheldAnswer,
        now_ms: i64,
        undo: &mut Undo,
    ) -> Option<Vec<OutboundSipEffect>> {
        self.prune(now_ms);
        undo.push((answer.key.clone(), self.entries.get(&answer.key).copied()));
        let entry = self.entries.get_mut(&answer.key).filter(|e| e.live(now_ms));
        let Some(entry) = entry else {
            self.make_room();
            let first = Entry { first_ms: now_ms, copies: 0, answered_ms: now_ms, last_ms: now_ms };
            self.entries.insert(answer.key.clone(), first);
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

    /// Put back each entry `undo` noted, the latest change first.
    fn undo(&mut self, undo: Undo) {
        for (key, before) in undo.into_iter().rev() {
            match before {
                Some(entry) => self.entries.insert(key, entry),
                None => self.entries.remove(&key),
            };
        }
    }
}

/// The answers to unheld finals this node sent, by final.
pub(crate) struct UnheldBook {
    inner: Mutex<Inner>,
}

impl Default for UnheldBook {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
}

impl UnheldBook {
    /// A book holding at most `capacity` finals.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let inner = Inner { entries: HashMap::new(), pruned_ms: 0, capacity: capacity.max(2) };
        Self { inner: Mutex::new(inner) }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Inner> {
        crate::poison::locked(&self.inner, crate::poison::UNHELD_ANSWERS)
    }

    /// What `answers`, drawn at `now_ms`, send, and the admission that takes
    /// the book's changes back unless [`Admission::sent`]; each entry is
    /// `None` for a copy absorbed.
    fn admit_all(
        &self,
        answers: Vec<UnheldAnswer>,
        now_ms: i64,
    ) -> (Vec<Option<Vec<OutboundSipEffect>>>, Admission<'_>) {
        let mut undo = Undo::new();
        if answers.is_empty() {
            return (Vec::new(), Admission { book: self, undo });
        }
        let mut inner = self.locked();
        let sent = answers.into_iter().map(|a| inner.admit(a, now_ms, &mut undo)).collect();
        (sent, Admission { book: self, undo })
    }
}

/// A turn's admission into the book: dropped before [`Admission::sent`], it
/// puts back every entry it changed, as if the turn's answers never drew.
#[must_use = "an admission dropped unsent takes its answers back"]
pub(super) struct Admission<'a> {
    book: &'a UnheldBook,
    undo: Undo,
}

impl Admission<'_> {
    /// The turn's answers reached the call's outbox: the book keeps them.
    pub(super) fn sent(mut self) {
        self.undo.clear();
    }
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if self.undo.is_empty() {
            return;
        }
        self.book.locked().undo(std::mem::take(&mut self.undo));
    }
}

/// What `sent`, one answer's admission, sends; an absorbed copy sends nothing
/// and is counted.
fn counted(ctx: &RouterCtx, sent: Option<Vec<OutboundSipEffect>>) -> Vec<OutboundSipEffect> {
    sent.unwrap_or_else(|| {
        ctx.metrics.record_copy_past_schedule(KIND);
        Vec::new()
    })
}

/// What `answer`, drawn at `now_ms` and sent at once, sends through the
/// node's book; an absorbed copy sends nothing and is counted.
pub(super) fn admitted(
    ctx: &RouterCtx,
    answer: UnheldAnswer,
    now_ms: i64,
) -> Vec<OutboundSipEffect> {
    let (sent, admission) = ctx.unheld_answers.admit_all(vec![answer], now_ms);
    admission.sent();
    sent.into_iter().flat_map(|s| counted(ctx, s)).collect()
}

/// `result` with what its unheld answers, drawn at `now_ms`, send moved onto
/// its outbound effects behind the turn's own, and the admission the commit
/// marks [`Admission::sent`] once the turn's entry is on the call's outbox.
pub(super) fn admit_turn(
    ctx: &RouterCtx,
    mut result: HandlerResult,
    now_ms: i64,
) -> (HandlerResult, Admission<'_>) {
    let answers = std::mem::take(&mut result.effects.unheld);
    let (sent, admission) = ctx.unheld_answers.admit_all(answers, now_ms);
    for s in sent {
        result.effects.outbound.extend(counted(ctx, s));
    }
    (result, admission)
}

#[cfg(test)]
#[path = "unheld_book_tests.rs"]
mod tests;
