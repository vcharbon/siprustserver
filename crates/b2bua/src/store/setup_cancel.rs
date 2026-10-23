//! The setup-CANCEL marks: the initial INVITEs the caller CANCELed while
//! their turn waited, keyed by call and CSeq.

use super::CallState;

impl CallState {
    /// Mark the initial INVITE of `call_ref` carrying CSeq `cseq` as
    /// CANCEL-finalized (the txn layer answered 200 + 487 and emitted
    /// `Cancelled`). Called by the router run loop at `Cancelled` ingress —
    /// BEFORE the event queues behind the initial-INVITE turn on the per-call
    /// FIFO — so that turn can see the caller is gone while the call model
    /// still reads `Active`. Idempotent.
    pub fn mark_setup_cancelled(&self, call_ref: &str, cseq: u32) {
        let mut inner = self.locked();
        match inner.setup_cancelled.get_mut(call_ref) {
            Some(marks) if marks.contains(&cseq) => {}
            Some(marks) => marks.push(cseq),
            None => {
                inner.setup_cancelled.insert(call_ref.to_string(), vec![cseq]);
            }
        }
    }

    /// Has the initial INVITE of `call_ref` carrying CSeq `cseq` been
    /// CANCELed (see [`mark_setup_cancelled`](Self::mark_setup_cancelled))?
    /// Read twice by that INVITE's turn, the caller already holding its 487:
    /// before the decision, a setup cancelled while the turn waited is born
    /// and asks no decision; at the decision-application seam, a route/reject
    /// landing on a call cancelled during the round trip is dropped.
    pub fn is_setup_cancelled(&self, call_ref: &str, cseq: u32) -> bool {
        self.locked().setup_cancelled.get(call_ref).is_some_and(|m| m.contains(&cseq))
    }

    /// Clear the setup-CANCEL mark of `call_ref`'s INVITE carrying CSeq
    /// `cseq`: its turn ended with no call of its own.
    pub fn clear_setup_cancelled(&self, call_ref: &str, cseq: u32) {
        self.retain_setup_cancelled(call_ref, |marked| marked != cseq);
    }

    /// Keep, of `call_ref`'s setup-CANCEL marks, those whose CSeq `waiting`
    /// says an INVITE still waits for its turn under: the router's release of
    /// a call clears the rest.
    pub fn retain_setup_cancelled(&self, call_ref: &str, waiting: impl Fn(u32) -> bool) {
        let mut inner = self.locked();
        let Some(marks) = inner.setup_cancelled.get_mut(call_ref) else { return };
        marks.retain(|cseq| waiting(*cseq));
        if marks.is_empty() {
            inner.setup_cancelled.remove(call_ref);
        }
    }
}
