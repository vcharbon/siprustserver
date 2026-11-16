//! Which finals fail a side's in-dialog requests: a non-2xx unless a BYE
//! ended the dialog first, a retry superseded it, or the loss model reordered
//! the request; and what is left unanswered.

use std::collections::{HashMap, HashSet};

use super::WireFailure;

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

    pub(super) fn sent(&mut self, cseq: u32) {
        self.highest = self.highest.max(cseq);
    }

    /// A 500 to the request `cseq` refuses a copy the loss model held back
    /// until a higher CSeq of the side reached the receiver: what it owes a
    /// CSeq going backwards (RFC 3261 §12.2.2).
    pub(super) fn reordered(&self, cseq: u32, code: u16) -> bool {
        code == 500 && self.lost.contains(&cseq) && self.highest > cseq
    }
}

/// Whether the non-2xx final `code` to an in-dialog request fails it: a 481
/// or 487 that reached a side after a BYE ended its dialog answers a request
/// still in flight when the dialog ended (RFC 3261 §15.1.2, §12.2.2).
pub(super) fn fails(code: u16, byed: bool) -> bool {
    code >= 300 && !(byed && matches!(code, 481 | 487))
}

/// Forget the 491 or 500 finals of `failures` to the `method` requests: the
/// side re-sent the re-INVITE or UPDATE they refused (RFC 3261 §14.1), and
/// the retry's own final judges it.
pub(super) fn retried(failures: &mut Vec<WireFailure>, method: &str) {
    if matches!(method, "INVITE" | "UPDATE") {
        failures.retain(|f| f.method != method || !matches!(f.code, Some(491 | 500)));
    }
}

/// After the seal, the final `code` to the request `method` `cseq`: a
/// request other than a BYE that `pending` still holds is settled, a non-2xx
/// one a failure unless the side's dialog had ended (`byed`, [`fails`]). A
/// late 481 is that request's failure only: the call's ending stays as the
/// seal left it, never `cut`.
pub(super) fn settle_pending(
    (pending, failures): (&mut HashMap<u32, (String, i64)>, &mut Vec<WireFailure>),
    byed: bool,
    method: &str,
    cseq: u32,
    code: u16,
    now: i64,
) {
    let held = pending.get(&cseq).is_some_and(|(m, _)| m == method && m != "BYE");
    if code < 200 || !held {
        return;
    }
    pending.remove(&cseq);
    if fails(code, byed) {
        failures.push(WireFailure { at_ms: now, method: method.to_string(), code: Some(code) });
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
