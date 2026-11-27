//! The resident calls a stuck poll holds, being discharged before the process
//! exits (ADR-0057 §6): every delete written for one states the discharge, so
//! the peer holding its copy refuses the call's identity for 64·T1.

use super::CallState;

impl CallState {
    /// Mark the resident `call_ref` discharged: the deletes written for it
    /// from now on state it. A call not resident is not marked.
    pub fn mark_discharging(&self, call_ref: &str) {
        let mut inner = self.locked();
        if inner.calls.contains_key(call_ref) {
            inner.discharging.insert(call_ref.to_string());
        }
    }
}
