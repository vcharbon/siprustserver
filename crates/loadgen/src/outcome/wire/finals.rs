//! Which finals fail a side's in-dialog requests: a non-2xx unless the
//! request was in flight as a BYE ended its dialog (or, for a 487, the leg
//! it was relayed toward), a retry the final asked
//! for superseded it, or the loss model reordered it; and what is left
//! unanswered.

use std::collections::{HashMap, HashSet};

use sip_message::header::retry_after_seconds;
use sip_message::sip_str::SipStr;
use sip_message::sniff::header_values;

use super::WireFailure;

/// RFC 3261 T2: how far apart a 487 and the BYE that ended its dialog arrive
/// at most when the request was pending as the dialog ended.
const T2_MS: i64 = 4_000;

/// The CSeq order of a side's in-dialog requests: the highest it sent, and
/// those the loss model dropped on their way out.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SentOrder {
    highest: u32,
    lost: HashSet<u32>,
}

impl SentOrder {
    /// The loss model dropped the request `cseq` on its way out.
    pub(super) fn lost(&mut self, cseq: u32) {
        self.lost.insert(cseq);
    }

    fn sent(&mut self, cseq: u32) {
        self.highest = self.highest.max(cseq);
    }

    /// A 500 to the request `cseq` refuses a copy the loss model held back
    /// until a higher CSeq of the side reached the receiver: what it owes a
    /// CSeq going backwards (RFC 3261 §12.2.2).
    fn reordered(&self, cseq: u32, code: u16) -> bool {
        code == 500 && self.lost.contains(&cseq) && self.highest > cseq
    }
}

/// One BYE that left the side (`sent`) or reached it, first seen at `at_ms`.
#[derive(Debug, Clone, PartialEq)]
struct Bye {
    sent: bool,
    cseq: u32,
    at_ms: i64,
}

/// One of a side's in-dialog requests, as its final judges it.
pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    pub(super) cseq: u32,
    pub(super) sent_at: i64,
}

impl Request<'_> {
    /// Whether `bye` is another BYE than this request itself.
    fn other_than(&self, bye: &Bye) -> bool {
        !(self.method == "BYE" && bye.sent && bye.cseq == self.cseq)
    }
}

/// What a side's in-dialog finals are judged against: the order of its
/// requests, the BYEs that left or reached it or another side of the call,
/// its refusals a retry may supersede, and its 487s a BYE arriving soon after
/// may still excuse.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Finals {
    pub(super) sent: SentOrder,
    byes: Vec<Bye>,
    /// When a BYE first left or reached another side of the call, ending the
    /// leg toward which the system under test relays the side's requests.
    other_byes: Vec<i64>,
    /// A 491, or a 500 with a Retry-After, to a re-INVITE or UPDATE: the
    /// side's next request of that method retries it (RFC 3261 §14.1).
    retriable: Vec<WireFailure>,
    /// Each 487 failure, with when its request was sent.
    terminated: Vec<(WireFailure, i64)>,
}

impl Finals {
    /// A BYE left the side (`sent`) or reached it at `now`: the 487 failures
    /// of `failures` that arrived at most T2 before it answered requests
    /// pending as it ended the dialog (RFC 3261 §15.1.2), and go. Returns
    /// whether this BYE is new to the side (not a retransmission).
    pub(super) fn bye(
        &mut self,
        failures: &mut Vec<WireFailure>,
        sent: bool,
        cseq: u32,
        now: i64,
    ) -> bool {
        let new = !self.byes.iter().any(|b| b.sent == sent && b.cseq == cseq);
        if new {
            self.byes.push(Bye { sent, cseq, at_ms: now });
        }
        self.forget_terminated(failures, now);
        new
    }

    /// A BYE first left or reached another side of the call at `now`: the
    /// side's 487 failures that arrived at most T2 before it answered
    /// requests relayed toward the leg it ended, and go.
    pub(super) fn other_bye(&mut self, failures: &mut Vec<WireFailure>, now: i64) {
        self.other_byes.push(now);
        self.forget_terminated(failures, now);
    }

    /// Forget the 487 failures of `failures` a BYE at `now` excuses: sent
    /// before it, arrived at most T2 before it.
    fn forget_terminated(&mut self, failures: &mut Vec<WireFailure>, now: i64) {
        self.terminated.retain(|(failure, sent_at)| {
            let ended = now >= *sent_at && now - failure.at_ms <= T2_MS;
            if ended {
                forget(failures, failure);
            }
            !ended
        });
    }

    /// The side sent an in-dialog request `method` `cseq`: the refusals of
    /// `failures` it retries go, the retry's own final judging it.
    pub(super) fn sent(&mut self, failures: &mut Vec<WireFailure>, method: &str, cseq: u32) {
        self.sent.sent(cseq);
        self.retriable.retain(|refusal| {
            let retried = refusal.method == method;
            if retried {
                forget(failures, refusal);
            }
            !retried
        });
    }

    /// The final `code` (`raw`) to the side's request `req` arrived at `now`:
    /// a failure joins `failures` unless a BYE other than the request ended
    /// its dialog first (a 481, RFC 3261 §12.2.2), a BYE on its dialog or on
    /// another side of the call came within T2 of it after it was sent (a
    /// 487, §15.1.2), or the 500 refuses a CSeq the loss model reordered.
    /// Returns whether it failed.
    pub(super) fn judge(
        &mut self,
        failures: &mut Vec<WireFailure>,
        req: &Request<'_>,
        (code, raw): (u16, &[u8]),
        now: i64,
    ) -> bool {
        let fails = match code {
            ..300 => false,
            481 => !self.byes.iter().any(|b| req.other_than(b)),
            487 => {
                let ended_near = |at: i64| at >= req.sent_at && now - at <= T2_MS;
                let own = self.byes.iter().any(|b| req.other_than(b) && ended_near(b.at_ms));
                !own && !self.other_byes.iter().any(|at| ended_near(*at))
            }
            _ => !self.sent.reordered(req.cseq, code),
        };
        if !fails {
            return false;
        }
        let failure = WireFailure { at_ms: now, method: req.method.to_string(), code: Some(code) };
        if code == 487 {
            self.terminated.push((failure.clone(), req.sent_at));
        }
        if retriable(req.method, code, raw) {
            self.retriable.push(failure.clone());
        }
        failures.push(failure);
        true
    }
}

/// Whether the refusal `code` (`raw`) of a `method` request asks its sender
/// to retry it: a 491 to a re-INVITE or UPDATE, or a 500 carrying a
/// Retry-After (RFC 3261 §14.1).
fn retriable(method: &str, code: u16, raw: &[u8]) -> bool {
    let retry_after = || {
        retry_after_seconds(header_values(raw, "Retry-After").into_iter().map(SipStr::from))
            .is_some()
    };
    matches!(method, "INVITE" | "UPDATE") && (code == 491 || code == 500 && retry_after())
}

/// Remove one failure equal to `failure` from `failures`.
fn forget(failures: &mut Vec<WireFailure>, failure: &WireFailure) {
    if let Some(i) = failures.iter().position(|f| f == failure) {
        failures.remove(i);
    }
}

/// After the seal, the final `code` (`raw`) to the request `method` `cseq`:
/// a request other than a BYE that `pending` still holds is settled, a
/// non-2xx one judged by `finals` ([`Finals::judge`]). A late 481 is that
/// request's failure only: the call's ending stays as the seal left it,
/// never `cut`.
pub(super) fn settle_pending(
    (pending, failures, finals): (
        &mut HashMap<u32, (String, i64)>,
        &mut Vec<WireFailure>,
        &mut Finals,
    ),
    method: &str,
    cseq: u32,
    (code, raw): (u16, &[u8]),
    now: i64,
) {
    let held = pending.get(&cseq).is_some_and(|(m, _)| m == method && m != "BYE");
    if code < 200 || !held {
        return;
    }
    if let Some((method, sent_at)) = pending.remove(&cseq) {
        finals.judge(failures, &Request { method: &method, cseq, sent_at }, (code, raw), now);
    }
}

/// The requests of `pending` other than a BYE, as failures without a code,
/// in the order they were sent.
pub(super) fn unanswered(pending: &HashMap<u32, (String, i64)>) -> Vec<WireFailure> {
    let mut left: Vec<WireFailure> = pending
        .values()
        .filter(|(method, _)| method != "BYE")
        .map(|(method, at_ms)| WireFailure { at_ms: *at_ms, method: method.clone(), code: None })
        .collect();
    left.sort_by_key(|f| f.at_ms);
    left
}

#[cfg(test)]
#[path = "finals_tests.rs"]
mod tests;
