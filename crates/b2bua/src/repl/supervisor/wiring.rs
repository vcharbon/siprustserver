//! What a host wires into every puller the supervisor spawns, before
//! [`start`](ReplicationSupervisor::start).

use super::{wiring, ReplicationSupervisor};

impl ReplicationSupervisor {
    /// Wire the router command sink every puller forwards X11 fail-back commands
    /// to (reactive reclaim / bulk reclaim). Call **before** [`start`](Self::start)
    /// so the initial pullers pick it up; a re-spawned puller reads it too.
    pub fn set_repl_sink(&self, sink: impl Into<crate::repl::RouterSink>) {
        *wiring(&self.inner.repl_tx) = Some(sink.into());
    }

    /// Wire the memory admission gate whose backup ceilings every puller
    /// applies (ADR-0037). Call **before** [`start`](Self::start).
    pub fn set_capacity(&self, gate: crate::capacity::CapacityGate) {
        *wiring(&self.inner.capacity) = Some(gate);
    }

    /// Wire the turn-point seam every puller reports an applied write to
    /// (ADR-0055 §13). Call **before** [`start`](Self::start).
    pub fn set_turn_probe(&self, probe: crate::turn_probe::TurnProbes) {
        *wiring(&self.inner.turn_probe) = probe;
    }
}
