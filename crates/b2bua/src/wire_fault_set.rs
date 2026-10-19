//! [`WireFaultSet`]: the wire faults armed when a turn starts (ADR-0052 §5),
//! a plain value the turn reads at each guarded emission.

/// A switchable wire fault, one per guarded emission site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFaultPoint {
    /// The in-dialog keepalive OPTIONS reuses the dialog's current local CSeq
    /// instead of advancing it (RFC 3261 §12.2.1.1) — the defect
    /// `cseq-in-dialog-order` names, invisible to a UA that answers what it is
    /// handed.
    KeepaliveCseqReuse,
}

/// The wire faults one turn emits. Production's is [`none`](Self::none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WireFaultSet {
    keepalive_cseq_reuse: bool,
}

impl WireFaultSet {
    /// No fault armed.
    pub fn none() -> Self {
        Self::default()
    }

    /// `self` with `point` armed or not.
    pub fn with(mut self, point: WireFaultPoint, armed: bool) -> Self {
        match point {
            WireFaultPoint::KeepaliveCseqReuse => self.keepalive_cseq_reuse = armed,
        }
        self
    }

    /// Whether `point`'s deviation is emitted.
    pub fn is_armed(&self, point: WireFaultPoint) -> bool {
        match point {
            WireFaultPoint::KeepaliveCseqReuse => self.keepalive_cseq_reuse,
        }
    }
}
