//! What the SUT serves live: its copy of a call and the store envelope
//! beside it (introspection).

use crate::B2buaSut;

impl B2buaSut {
    /// The live copy of `call_ref` this worker serves, if any (introspection:
    /// what its rules read at the next event).
    pub fn live_call(&self, call_ref: &str) -> Option<call::Call> {
        self._core.live_call(call_ref)
    }

    /// The store envelope of the live copy of `call_ref`, if any.
    pub fn live_topology(&self, call_ref: &str) -> Option<b2bua::store::CallTopology> {
        self._core.live_topology(call_ref)
    }
}
