//! The store envelope a resident call keeps beside it (ADR-0055 §7, §11):
//! written as the call enters the map and by a meeting, read by the shell.

use std::sync::Arc;

use super::{CallState, CallTopology, Enveloped, Inner};

impl CallState {
    /// [`create`](Self::create) `call`, placed under the worker pair its
    /// caller's INVITE cookie names, as the router places a new call.
    #[cfg(any(test, feature = "testkit"))]
    pub fn create_placed(&self, call: call::Call) -> String {
        let topology = crate::router::topology_cookie::placement(&call);
        self.create(call, topology)
    }

    /// Write `call_ref`'s envelope: `topology`, or none.
    pub(super) fn place(inner: &mut Inner, call_ref: &str, topology: Option<CallTopology>) {
        match topology {
            Some(t) => {
                inner.topologies.insert(call_ref.to_string(), t);
            }
            None => {
                inner.topologies.remove(call_ref);
            }
        }
    }

    /// The store envelope of the resident call `call_ref`; `None` when no call
    /// is resident or it was placed under no worker pair.
    pub fn topology(&self, call_ref: &str) -> Option<CallTopology> {
        self.locked().topologies.get(call_ref).cloned()
    }

    /// The resident call `call_ref` in its envelope (clone), if present.
    pub fn peek_enveloped(&self, call_ref: &str) -> Option<Enveloped> {
        let (call, topology) = {
            let inner = self.locked();
            (inner.calls.get(call_ref).cloned()?, inner.topologies.get(call_ref).cloned())
        };
        Some(Enveloped::new(Arc::unwrap_or_clone(call), topology))
    }

    /// Replace the in-memory call and its envelope with `copy`, a meeting's
    /// outcome: the replace-only rule of [`update_quiet`](Self::update_quiet),
    /// the envelope written as `copy` holds it.
    pub fn update_enveloped(&self, copy: Enveloped) {
        let keys = call::call_index_keys(&copy.call);
        let mut inner = self.locked();
        if !inner.calls.contains_key(&copy.call.call_ref) {
            return;
        }
        Self::place(&mut inner, &copy.call.call_ref, copy.topology);
        let released = self.replace_locked(&mut inner, Arc::new(copy.call), keys);
        drop(inner);
        drop(released);
    }
}
