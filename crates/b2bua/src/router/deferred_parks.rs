//! The calls a bulk reclaim parked on a turn of their own because a turn held
//! their lock, as ingress sees them until their deferred arming ran. A copy of
//! such a call's initial INVITE is the call's: it queues on the call's queue,
//! unjudged, keeping the transaction the layer opened for it, and is judged at
//! its turn (`super::deferred_copy`). A CANCEL of that INVITE marks its setup
//! cancelled, as one of a resident call's does. A request naming no `callRef`
//! of ours finds the call through the store's SIP index, which the bodies the
//! reclaim reads carry.

use std::collections::HashMap;

use call::{Call, IndexLookup};
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

    /// Whether `req` copies the initial INVITE of a deferred call: its INVITE
    /// CSeq, and its top-Via branch naming the call's own server transaction.
    pub(super) fn copies(&self, req: &SipRequest) -> bool {
        if self.calls.is_empty() {
            return false;
        }
        let call_id = req.call_id().as_str();
        let from_tag = req.from().tag().unwrap_or("");
        let lookup = IndexLookup::Invite { call_id, from_tag };
        let Some(hit) = resolve_in(&self.index, &lookup, &mut lookup.key_buffer()) else {
            return false;
        };
        let Some(deferred) = self.calls.get(&hit.call_ref) else { return false };
        let branch = req.top_via().branch().unwrap_or_default();
        let call = &deferred.call;
        call.a_leg_invite.cseq == req.cseq().seq() && names_server_branch(call, branch)
    }

    /// Whether `cseq` is the CSeq of the initial INVITE of the deferred
    /// `call_ref`.
    pub(super) fn invite_here(&self, call_ref: &str, cseq: u32) -> bool {
        self.calls.get(call_ref).is_some_and(|deferred| deferred.call.a_leg_invite.cseq == cseq)
    }
}

#[cfg(test)]
mod tests;
