//! The calls a bulk reclaim parked on a turn of their own because a turn held
//! their lock, as ingress sees them until their deferred arming ran: a copy
//! of such a call's initial INVITE is the call's. Once the arming is queued,
//! the copy keeps the transaction the layer opened for it and queues behind
//! the arming, which rebuilds the INVITE's own transaction in its place and
//! asks an undecided call's decision again; the copy's re-offer meets that
//! transaction, and a call gone by then answers the copy on the transaction
//! it kept. Before, the copy queues behind the park, unjudged, and is
//! answered as a parked call answers one. A request naming no `callRef` of
//! ours finds the call through the store's SIP index, which the bodies the
//! reclaim reads carry.

use std::collections::HashMap;

use call::{Call, IndexHit, IndexLookup};
use sip_message::SipRequest;

use super::materialise::names_server_branch;
use crate::store::resolve_in;

/// One deferred call.
struct Deferred {
    /// The body the bulk reclaim scanned.
    call: Call,
    /// The call's arming is queued behind its park.
    arm_queued: bool,
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
    /// The call's arming is queued: the copy queues behind it.
    pub(super) arm_queued: bool,
}

impl DeferredParks {
    /// The call the bulk reclaim scanned as `call` waits for its deferred
    /// park; a call already deferred stays as it is.
    pub(super) fn defer(&mut self, call: Call) {
        if self.calls.contains_key(&call.call_ref) {
            return;
        }
        for key in call::call_index_keys(&call) {
            self.index.insert(key, call.call_ref.clone());
        }
        let call_ref = call.call_ref.clone();
        self.calls.insert(call_ref, Deferred { call, arm_queued: false });
    }

    /// The arming of `call_ref` is queued behind its park.
    pub(super) fn arming(&mut self, call_ref: &str) {
        if let Some(deferred) = self.calls.get_mut(call_ref) {
            deferred.arm_queued = true;
        }
    }

    /// The deferred arming of `call_ref` met the node's endpoint not reading
    /// ready: the call waits for the next one.
    pub(super) fn held_back(&mut self, call_ref: &str) {
        if let Some(deferred) = self.calls.get_mut(call_ref) {
            deferred.arm_queued = false;
        }
    }

    /// The deferred arming of `call_ref` ran, or met the call's released
    /// queue: the call is resident, or gone.
    pub(super) fn armed(&mut self, call_ref: &str) {
        let Some(deferred) = self.calls.remove(call_ref) else { return };
        for key in call::call_index_keys(&deferred.call) {
            if self.index.get(&key).is_some_and(|owner| owner == call_ref) {
                self.index.remove(&key);
            }
        }
    }

    /// The scanned bodies of the deferred calls whose arming is not queued.
    pub(super) fn unarmed(&self) -> impl Iterator<Item = &Call> {
        self.calls.values().filter(|deferred| !deferred.arm_queued).map(|deferred| &deferred.call)
    }

    /// The deferred call whose initial INVITE `req` copies: its INVITE CSeq,
    /// and its top-Via branch naming the call's own server transaction.
    pub(super) fn copied(&self, req: &SipRequest) -> Option<Copied> {
        if self.calls.is_empty() {
            return None;
        }
        let call_id = req.call_id().as_str();
        let from_tag = req.from().tag().unwrap_or("");
        let lookup = IndexLookup::Invite { call_id, from_tag };
        let hit = resolve_in(&self.index, &lookup, &mut lookup.key_buffer())?;
        let deferred = self.calls.get(&hit.call_ref)?;
        let branch = req.top_via().branch().unwrap_or_default();
        let call = &deferred.call;
        let copies =
            call.a_leg_invite.cseq == req.cseq().seq() && names_server_branch(call, branch);
        copies.then_some(Copied { hit, arm_queued: deferred.arm_queued })
    }
}

#[cfg(test)]
mod tests;
