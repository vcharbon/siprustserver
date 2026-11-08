//! Test hooks into the call map's lock, for the tests of what other locks
//! a caller holds while it waits on this one.

use super::CallState;

impl CallState {
    /// Run `f` while holding the call map's lock.
    pub(crate) fn holding_call_map<R>(&self, f: impl FnOnce() -> R) -> R {
        let _inner = self.locked();
        f()
    }
}
