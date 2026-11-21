//! The reads of an INVITE client transaction's counters: the CANCEL it holds
//! until a provisional (RFC 3261 §9.1) and the 2xx it owes nothing (RFC 6026
//! §7.2).

use std::sync::atomic::Ordering;

use super::TransactionMetrics;

impl TransactionMetrics {
    /// CANCELs held back awaiting their INVITE's first provisional (§9.1).
    pub fn cancels_held(&self) -> u64 {
        self.inner.cancels_held.load(Ordering::Relaxed)
    }

    /// Held CANCELs flushed to the wire on the first provisional.
    pub fn held_cancels_flushed(&self) -> u64 {
        self.inner.held_cancels_flushed.load(Ordering::Relaxed)
    }

    /// Held CANCELs cleared without ever reaching the wire (a final came
    /// first, or the txn died with no provisional).
    pub fn held_cancels_dropped(&self) -> u64 {
        self.inner.held_cancels_dropped.load(Ordering::Relaxed)
    }

    /// CANCELs suppressed at send: the INVITE txn had taken its final or was gone.
    pub fn cancels_suppressed_on_final(&self) -> u64 {
        self.inner.cancels_suppressed_on_final.load(Ordering::Relaxed)
    }

    /// 2xx responses absorbed by a re-INVITE client txn that owes them nothing
    /// (counter).
    pub fn late_2xx_dropped(&self) -> u64 {
        self.inner.late_2xx_dropped.load(Ordering::Relaxed)
    }
}
