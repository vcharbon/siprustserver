//! A worker's acceptance of new calls gated on a backend's breaker, as a host
//! wires it for a backend new calls depend on (ADR-0050 §9).

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
}
