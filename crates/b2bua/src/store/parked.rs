//! The reclaimed calls this node holds resident but does not serve yet
//! (ADR-0055 §8): materialised and written while the node's own endpoint does
//! not read ready, with no timer armed and no transaction seeded, so nothing
//! of theirs leaves the node until they are armed.

use super::CallState;

impl CallState {
    /// Mark the resident `call_ref` parked: reclaimed, not yet armed.
    pub fn park(&self, call_ref: &str) {
        self.locked().parked.insert(call_ref.to_string());
    }

    /// Clear `call_ref`'s parked mark; whether it was parked.
    pub fn unpark(&self, call_ref: &str) -> bool {
        self.locked().parked.remove(call_ref)
    }

    /// Whether `call_ref` is parked.
    pub fn is_parked(&self, call_ref: &str) -> bool {
        self.locked().parked.contains(call_ref)
    }

    /// The parked callRefs, in no order.
    pub fn parked(&self) -> Vec<String> {
        self.locked().parked.iter().cloned().collect()
    }
}
