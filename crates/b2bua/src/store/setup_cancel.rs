//! The setup-CANCEL marks: the initial INVITEs the caller CANCELed while
//! their turn waited, keyed by call and CSeq, and the watch a turn awaiting
//! its decision holds on its own INVITE's mark.

use std::sync::Arc;

use tokio::sync::Notify;

use super::CallState;

/// A turn's watch on the setup-CANCEL mark of its initial INVITE: resolves
/// [`marked`](Self::marked) once the mark is set, whether before or after the
/// watch was taken. Dropping it forgets the watch.
pub struct SetupCancelWatch<'a> {
    state: &'a CallState,
    key: (String, u32),
    notify: Arc<Notify>,
}

impl SetupCancelWatch<'_> {
    /// Resolves once the INVITE's setup-CANCEL mark is set.
    pub async fn marked(&self) {
        self.notify.notified().await;
    }
}

impl Drop for SetupCancelWatch<'_> {
    fn drop(&mut self) {
        let mut inner = self.state.locked();
        let own =
            inner.setup_cancel_watches.get(&self.key).is_some_and(|w| Arc::ptr_eq(w, &self.notify));
        if own {
            inner.setup_cancel_watches.remove(&self.key);
        }
    }
}

impl CallState {
    /// Mark the initial INVITE of `call_ref` carrying CSeq `cseq` as
    /// CANCEL-finalized (the txn layer took its 487 and emitted
    /// `Cancelled`). Called by the router run loop at `Cancelled` ingress —
    /// BEFORE the event queues behind the initial-INVITE turn on the per-call
    /// FIFO — so that turn can see the caller is gone while the call model
    /// still reads `Active`, and a turn [watching](Self::watch_setup_cancel)
    /// the mark is woken. Idempotent.
    pub fn mark_setup_cancelled(&self, call_ref: &str, cseq: u32) {
        let mut inner = self.locked();
        match inner.setup_cancelled.get_mut(call_ref) {
            Some(marks) if marks.contains(&cseq) => {}
            Some(marks) => marks.push(cseq),
            None => {
                inner.setup_cancelled.insert(call_ref.to_string(), vec![cseq]);
            }
        }
        if let Some(watch) = inner.setup_cancel_watches.get(&(call_ref.to_string(), cseq)) {
            watch.notify_one();
        }
    }

    /// Watch the setup-CANCEL mark of `call_ref`'s initial INVITE carrying
    /// CSeq `cseq`, as its turn does while its decision runs. A mark already
    /// set resolves the watch at once. One watch per INVITE: a second
    /// replaces the first.
    pub fn watch_setup_cancel(&self, call_ref: &str, cseq: u32) -> SetupCancelWatch<'_> {
        let key = (call_ref.to_string(), cseq);
        let notify = Arc::new(Notify::new());
        let mut inner = self.locked();
        if inner.setup_cancelled.get(call_ref).is_some_and(|m| m.contains(&cseq)) {
            notify.notify_one();
        }
        inner.setup_cancel_watches.insert(key.clone(), notify.clone());
        drop(inner);
        SetupCancelWatch { state: self, key, notify }
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

    /// The number of live setup-CANCEL marks. Cleared by every release of the
    /// call that no waiting INVITE outlives; a residue after teardown is a
    /// mark leak (the harness `assert_fully_reaped` 5th invariant).
    pub fn setup_cancelled_count(&self) -> usize {
        self.locked().setup_cancelled.values().map(Vec::len).sum()
    }

    /// The watches turns hold on their INVITE's mark.
    pub fn setup_cancel_watch_count(&self) -> usize {
        self.locked().setup_cancel_watches.len()
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
