//! Injectable WIRE-fault seam: a control handle a test flips so the B2BUA emits
//! one named RFC deviation on purpose. It exists so the post-run RFC audit can
//! be proven to REACH its gate — a zero-finding run proves only that zero
//! findings pass. Default = no faults, a no-op atomic read on the guarded path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A switchable wire fault, one per guarded emission site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFaultPoint {
    /// The in-dialog keepalive OPTIONS reuses the dialog's current local CSeq
    /// instead of advancing it (RFC 3261 §12.2.1.1) — the defect
    /// `cseq-in-dialog-order` names, invisible to a UA that answers what it is
    /// handed.
    KeepaliveCseqReuse,
}

#[derive(Default)]
struct Inner {
    keepalive_cseq_reuse: AtomicBool,
}

/// Shared, clone-cheap handle. Production passes `Default` and never arms it.
#[derive(Clone, Default)]
pub struct WireFaults {
    inner: Arc<Inner>,
}

impl WireFaults {
    /// A fresh all-disarmed handle (alias of `Default`).
    pub fn none() -> Self {
        Self::default()
    }

    fn cell(&self, point: WireFaultPoint) -> &AtomicBool {
        match point {
            WireFaultPoint::KeepaliveCseqReuse => &self.inner.keepalive_cseq_reuse,
        }
    }

    /// Emit `point`'s deviation from now on (until [`disarm`](Self::disarm)ed).
    pub fn arm(&self, point: WireFaultPoint) {
        self.cell(point).store(true, Ordering::Relaxed);
    }

    /// Restore `point` to compliant emission.
    pub fn disarm(&self, point: WireFaultPoint) {
        self.cell(point).store(false, Ordering::Relaxed);
    }

    pub fn is_armed(&self, point: WireFaultPoint) -> bool {
        self.cell(point).load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_handle_is_disarmed_and_a_clone_shares_the_switch() {
        let faults = WireFaults::none();
        let twin = faults.clone();
        assert!(!twin.is_armed(WireFaultPoint::KeepaliveCseqReuse));
        faults.arm(WireFaultPoint::KeepaliveCseqReuse);
        assert!(twin.is_armed(WireFaultPoint::KeepaliveCseqReuse));
        twin.disarm(WireFaultPoint::KeepaliveCseqReuse);
        assert!(!faults.is_armed(WireFaultPoint::KeepaliveCseqReuse));
    }
}
