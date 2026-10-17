//! A worker's acceptance of new calls: gated on a backend's breaker, as a host
//! wires it for a backend new calls depend on, or withheld by an operator's
//! cordon (ADR-0050 §9).

use tokio::sync::watch;

use super::ReplicatedB2buaSut;

impl ReplicatedB2buaSut {
    /// Refuse new calls on this incarnation's OPTIONS answer while `breaker`
    /// reads open (`true`). A reboot spawns a new incarnation the gate does
    /// not follow. No-op while crashed.
    pub fn gate_options_on(&self, breaker: watch::Receiver<bool>) {
        if let Some(core) = &self.core {
            core.readiness().gate_options_on(breaker);
        }
    }

    /// Cordon this incarnation, as the operator's `POST /admin/cordon` does.
    /// A reboot spawns a new incarnation the cordon does not follow. Panics
    /// while crashed, or when no cordon applies (NotReady, Draining).
    pub fn cordon(&self) {
        let core = self.core.as_ref().expect("cordon of a crashed worker");
        core.readiness().cordon().expect("a ready worker takes a cordon");
    }

    /// Lift this incarnation's cordon, as `DELETE /admin/cordon` does. Panics
    /// while crashed or draining.
    pub fn uncordon(&self) {
        let core = self.core.as_ref().expect("uncordon of a crashed worker");
        core.readiness().uncordon().expect("a worker not draining is uncordoned");
    }
}
