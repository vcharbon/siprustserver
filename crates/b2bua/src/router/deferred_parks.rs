//! The calls a bulk reclaim parked on a turn of their own because a turn held
//! their lock, as ingress sees them until their deferred arming ran. A request
//! naming no `callRef` of ours finds such a call from the body the reclaim
//! scanned while it is not resident, and queues behind its park, as it would
//! reach the parked call. A copy of the call's initial INVITE waits until the
//! arming is queued, then queues behind it: the arming seeds the INVITE's
//! transaction and asks an undecided call's decision again, and the copy's
//! re-offer then meets that transaction.

use std::collections::HashMap;

use call::{Call, IndexHit, IndexLookup};
use sip_message::SipRequest;

use super::materialise::{names_server_branch, names_unanswered_caller_invite};
use crate::store::resolve_in;
use b2bua_sdk::event::CallEvent;

/// One deferred call.
struct Deferred {
    /// The body the bulk reclaim scanned.
    call: Call,
    /// The call's arming is queued behind its park.
    arm_queued: bool,
    /// The copies of its initial INVITE that wait for the arming to be queued.
    held: Vec<CallEvent>,
}

/// The router loop's deferred calls ([module doc](self)).
#[derive(Default)]
pub(super) struct DeferredParks {
    calls: HashMap<String, Deferred>,
    /// The SIP index keys of the scanned bodies → their callRef.
    index: HashMap<String, String>,
}

/// What a deferred call is to an initial INVITE that copies its own.
pub(super) struct Copied {
    pub(super) hit: IndexHit,
    /// It copies the caller's unanswered initial INVITE.
    pub(super) unanswered: bool,
    /// The call's arming is queued: the copy queues behind it.
    pub(super) arm_queued: bool,
}

impl DeferredParks {
    /// The call the bulk reclaim scanned as `call` waits for its deferred
    /// park; a call already deferred keeps what it holds.
    pub(super) fn defer(&mut self, call: Call) {
        if self.calls.contains_key(&call.call_ref) {
            return;
        }
        for key in call::call_index_keys(&call) {
            self.index.insert(key, call.call_ref.clone());
        }
        let call_ref = call.call_ref.clone();
        self.calls.insert(call_ref, Deferred { call, arm_queued: false, held: Vec::new() });
    }

    /// The arming of `call_ref` is queued (`queued`) or was discarded with
    /// its queue: the copies its INVITE held, to take in again. A call whose
    /// arming was discarded is no longer deferred.
    pub(super) fn arming(&mut self, call_ref: &str, queued: bool) -> Vec<CallEvent> {
        if !queued {
            return self.forget(call_ref);
        }
        let Some(deferred) = self.calls.get_mut(call_ref) else { return Vec::new() };
        deferred.arm_queued = true;
        std::mem::take(&mut deferred.held)
    }

    /// [`arming`](Self::arming) of each call an arming pass reports, with
    /// whether its arming was queued: every copy held, in order.
    pub(super) fn arming_all(&mut self, arming: Vec<(String, bool)>) -> Vec<CallEvent> {
        arming.into_iter().flat_map(|(call_ref, queued)| self.arming(&call_ref, queued)).collect()
    }

    /// The deferred arming of `call_ref` ran: the call is resident, or gone.
    pub(super) fn armed(&mut self, call_ref: &str) {
        self.forget(call_ref);
    }

    /// Hold `event`, a copy of the initial INVITE of the deferred `call_ref`
    /// whose arming is not queued yet.
    pub(super) fn hold(&mut self, call_ref: &str, event: CallEvent) {
        if let Some(deferred) = self.calls.get_mut(call_ref) {
            deferred.held.push(event);
        }
    }

    /// The deferred call a message naming no `callRef` of ours resolves to by
    /// `lookup`.
    pub(super) fn find(&self, lookup: IndexLookup<'_>) -> Option<IndexHit> {
        if self.calls.is_empty() {
            return None;
        }
        let mut key = lookup.key_buffer();
        resolve_in(&self.index, &lookup, &mut key)
    }

    /// The deferred call whose initial INVITE `req` copies: its INVITE CSeq,
    /// and its top-Via branch naming the call's own server transaction.
    pub(super) fn copied(&self, req: &SipRequest) -> Option<Copied> {
        let call_id = req.call_id().as_str();
        let from_tag = req.from().tag().unwrap_or("");
        let hit = self.find(IndexLookup::Invite { call_id, from_tag })?;
        let deferred = self.calls.get(&hit.call_ref)?;
        let branch = req.top_via().branch().unwrap_or_default();
        let call = &deferred.call;
        if call.a_leg_invite.cseq != req.cseq().seq() || !names_server_branch(call, branch) {
            return None;
        }
        let unanswered = names_unanswered_caller_invite(call, branch);
        Some(Copied { hit, unanswered, arm_queued: deferred.arm_queued })
    }

    /// Stop deferring `call_ref`: what its INVITE held.
    fn forget(&mut self, call_ref: &str) -> Vec<CallEvent> {
        let Some(deferred) = self.calls.remove(call_ref) else { return Vec::new() };
        for key in call::call_index_keys(&deferred.call) {
            if self.index.get(&key).is_some_and(|owner| owner == call_ref) {
                self.index.remove(&key);
            }
        }
        deferred.held
    }
}

#[cfg(test)]
mod tests;
