//! RFC 3261 §17.1.1.3 UAS-side ACK obligations: when this endpoint answers an
//! INVITE with a **non-2xx final**, the arriving hop ACK belongs to the INVITE
//! server transaction, not to the transaction user.
//!
//! Keyed `(Call-ID, INVITE top-Via branch)`, exactly like the SUT's own
//! synthesized hop ACK and the `unacked-invite-non-2xx-final` audit rule —
//! so the obligation, the wire and the audit can never disagree on what
//! matches.
//!
//! **Order-independence is the point**: the ACK races the next transaction's
//! INVITE (the reroute shape: hop ACK for the 486 vs the rerouted INVITE) and
//! may land before or after it. Matching is by key, never positional.

use std::collections::HashMap;
use std::sync::Mutex;

/// The per-endpoint ledger of open and fulfilled hop-ACK obligations.
#[derive(Default)]
pub(super) struct HopAcks {
    /// `(Call-ID, INVITE top-Via branch)` → the hop ACK has been sighted.
    pending: Mutex<HashMap<(String, String), bool>>,
    /// Wakes a waiter parked on fulfilment.
    notify: tokio::sync::Notify,
}

impl HopAcks {
    /// Open — or refresh, since a retransmitted final re-arms the same key
    /// without clearing a sighting — the obligation for one rejected INVITE.
    pub(super) fn arm(&self, call_id: String, branch: String) {
        self.pending.lock().unwrap().entry((call_id, branch)).or_insert(false);
    }

    /// Record an ACK sighting. Returns `true` iff the key belongs to an armed
    /// obligation (fulfilled now or previously) — the caller may absorb it.
    pub(super) fn note(&self, call_id: &str, branch: &str) -> bool {
        let mut g = self.pending.lock().unwrap();
        match g.get_mut(&(call_id.to_string(), branch.to_string())) {
            Some(seen) => {
                *seen = true;
                drop(g);
                self.notify.notify_waiters();
                true
            }
            None => false,
        }
    }

    pub(super) fn is_fulfilled(&self, call_id: &str, branch: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .get(&(call_id.to_string(), branch.to_string()))
            .copied()
            .unwrap_or(false)
    }

    /// Park until the obligation is fulfilled — WITHOUT pulling from the inbox,
    /// so a reactor whose own receive claims the ACK below its API still
    /// observes the fulfilment. Never times out; callers bound it.
    pub(super) async fn fulfilled(&self, call_id: &str, branch: &str) {
        loop {
            // Register interest BEFORE the check: `notify_waiters` only wakes
            // already-registered waiters, so check-then-wait would race a
            // sighting landing in between.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_fulfilled(call_id, branch) {
                return;
            }
            notified.await;
        }
    }
}
