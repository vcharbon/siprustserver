//! The self-release watches (ADR-0014): the calls the consumer asked to hear
//! of once no transaction serves them, each with its legs' Call-IDs. What
//! serves a call is what the self-release count reads: a transaction on its
//! books, or an unattributed in-dialog server transaction of one of those
//! Call-IDs that has sent no final (`layer::unattributed`). A watch holds while
//! either is left; the clear of the last one queues the call's one-shot
//! `CallQuiesced` for the end of the owner's turn (`layer::events`).

use std::collections::{HashMap, HashSet};

use super::owner::Owner;

/// The watched calls, each with the Call-IDs its watch names, and by Call-ID
/// the calls whose watch names it. One entry per watch, gone when it fires or
/// its call is relinquished.
#[derive(Default)]
pub(super) struct SelfReleaseWatch {
    calls: HashMap<String, Vec<String>>,
    by_call_id: HashMap<String, HashSet<String>>,
}

impl SelfReleaseWatch {
    /// Watch `call_ref` with `call_ids`, in place of a watch it had.
    fn arm(&mut self, call_ref: String, call_ids: Vec<String>) {
        self.disarm(&call_ref);
        for call_id in &call_ids {
            self.by_call_id.entry(call_id.clone()).or_default().insert(call_ref.clone());
        }
        self.calls.insert(call_ref, call_ids);
    }

    /// Stop watching `call_ref`; a call not watched changes nothing.
    pub(super) fn disarm(&mut self, call_ref: &str) {
        let Some(call_ids) = self.calls.remove(call_ref) else { return };
        for call_id in call_ids {
            let Some(calls) = self.by_call_id.get_mut(&call_id) else { continue };
            calls.remove(call_ref);
            if calls.is_empty() {
                self.by_call_id.remove(&call_id);
            }
        }
    }

    /// The watched calls whose watch names `call_id`.
    fn naming(&self, call_id: &str) -> Vec<String> {
        self.by_call_id.get(call_id).map_or_else(Vec::new, |c| c.iter().cloned().collect())
    }
}

impl Owner {
    /// Watch `call_ref`, whose legs' Call-IDs are `call_ids`, for the clear of
    /// the last transaction serving it; `CallQuiesced` at once, on the
    /// lossless path, when none serves it now.
    pub(super) fn watch_self_release(&mut self, call_ref: String, call_ids: Vec<String>) {
        if self.serves(&call_ref, &call_ids) {
            self.self_release_watch.arm(call_ref, call_ids);
        } else {
            self.notify_quiesced(call_ref);
        }
    }

    /// Whether `call_ref` is watched and nothing serves it any more.
    pub(super) fn quiesced(&self, call_ref: &str) -> bool {
        self.self_release_watch
            .calls
            .get(call_ref)
            .is_some_and(|call_ids| !self.serves(call_ref, call_ids))
    }

    /// A watched call left with nothing serving it hears `CallQuiesced` at
    /// the end of the turn.
    pub(super) fn quiesce_if_last(&mut self, call_ref: String) {
        if self.quiesced(&call_ref) {
            self.pending_quiesce.push(call_ref);
        }
    }

    /// An unattributed server transaction of `call_id` sent its final, was
    /// attributed, or left the map: every watched call naming `call_id` is
    /// checked again.
    pub(super) fn unattributed_settled(&mut self, call_id: &str) {
        for call_ref in self.self_release_watch.naming(call_id) {
            self.quiesce_if_last(call_ref);
        }
    }

    /// Whether a transaction serves `call_ref`: one on its books, or an
    /// unattributed one of `call_ids` that has sent no final.
    fn serves(&self, call_ref: &str, call_ids: &[String]) -> bool {
        self.has_txns_for(call_ref) || self.unattributed_serving(call_ids) != 0
    }
}
