//! A worker's release of every call it serves, as an operator asks for it
//! (ADR-0050 §10).

use b2bua::release_all::ReleasedAll;

use super::ReplicatedB2buaSut;

impl ReplicatedB2buaSut {
    /// Release every call this incarnation serves, as the operator's
    /// `POST /admin/release-all` does. Panics while crashed.
    pub fn release_all(&self) -> ReleasedAll {
        self.core.as_ref().expect("release-all of a crashed worker").release_all()
    }
}
