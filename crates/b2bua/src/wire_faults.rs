//! Injectable WIRE-fault seam: a control handle a test flips so the B2BUA emits
//! one named RFC deviation on purpose. It exists so the post-run RFC audit can
//! be proven to REACH its gate — a zero-finding run proves only that zero
//! findings pass. Default = no faults. A turn reads the handle once, as it
//! starts, into a [`WireFaultSet`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use crate::wire_fault_set::{WireFaultPoint, WireFaultSet};

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

    /// The faults armed now, as a turn starting now reads them.
    pub fn snapshot(&self) -> WireFaultSet {
        let point = WireFaultPoint::KeepaliveCseqReuse;
        WireFaultSet::none().with(point, self.is_armed(point))
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

    #[test]
    fn a_snapshot_holds_what_was_armed_when_it_was_taken() {
        let faults = WireFaults::none();
        faults.arm(WireFaultPoint::KeepaliveCseqReuse);
        let taken = faults.snapshot();
        faults.disarm(WireFaultPoint::KeepaliveCseqReuse);
        assert!(taken.is_armed(WireFaultPoint::KeepaliveCseqReuse));
        assert!(!faults.snapshot().is_armed(WireFaultPoint::KeepaliveCseqReuse));
    }
}
