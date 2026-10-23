//! What an operator reads and does on the SUT (ADR-0050 §9-10): its
//! readiness, the cordon included, and the release of every call it serves.

use b2bua::release_all::ReleasedAll;
use b2bua::repl::Readiness;

use crate::B2buaSut;

impl B2buaSut {
    /// The SUT's readiness handle: its OPTIONS self-health and the state
    /// `/ready` reads.
    pub fn readiness(&self) -> Readiness {
        self._core.readiness()
    }

    /// Release every call the SUT serves, as `POST /admin/release-all` does.
    pub fn release_all(&self) -> ReleasedAll {
        self._core.release_all()
    }
}
