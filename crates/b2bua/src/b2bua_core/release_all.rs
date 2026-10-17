//! The operator's release-all ([`crate::release_all`]) as the worker's host
//! reaches it.

use std::sync::Arc;

use super::B2buaCore;
use crate::release_all::{release_all, ReleaseAllFn, ReleasedAll};

impl B2buaCore {
    /// Release every call this worker serves (ADR-0050 §10). Each call is
    /// released on its own turn after this returns; the counts are what the
    /// request reached.
    pub fn release_all(&self) -> ReleasedAll {
        (self.release_all_handle())()
    }

    /// [`release_all`](Self::release_all) as an owned handle, run as this
    /// worker's node.
    pub fn release_all_handle(&self) -> ReleaseAllFn {
        let (ctx, node) = (self.ctx.clone(), self.node.clone());
        Arc::new(move || node.enter(|| release_all(&ctx)))
    }
}
