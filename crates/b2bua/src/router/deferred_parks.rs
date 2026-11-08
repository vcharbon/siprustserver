//! The calls a bulk reclaim parked on a turn of their own because a turn held
//! their lock, as ingress sees them until their deferred arming ran. A copy
//! of such a call's initial INVITE keeps the transaction the layer opened for
//! it and waits until the arming is queued, then queues behind it: the
//! arming rebuilds the INVITE's own transaction in its place and asks an
//! undecided call's decision again, and the copy's re-offer meets that
//! transaction; a call gone by then answers the copy on the transaction it
//! kept. A request naming no `callRef` of ours finds the call through the
//! store's SIP index, which the bodies the reclaim reads carry.

use std::collections::HashMap;

use call::{Call, IndexHit, IndexLookup};
use sip_message::SipRequest;

use super::materialise::names_server_branch;
use crate::store::resolve_in;
use b2bua_sdk::event::CallEvent;

/// One deferred call.
struct Deferred {
    /// The body the bulk reclaim scanned.
    call: Call,
    /// The call's arming is queued behind its park, or met a released queue.
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

    /// The arming of `call_ref` is queued, or met its released queue: the
    /// copies its INVITE held, to take in again behind it.
    pub(super) fn arming(&mut self, call_ref: &str) -> Vec<CallEvent> {
        let Some(deferred) = self.calls.get_mut(call_ref) else { return Vec::new() };
        deferred.arm_queued = true;
        std::mem::take(&mut deferred.held)
    }

    /// The deferred arming of `call_ref` met the node's endpoint not reading
    /// ready: the call waits for the next one.
    pub(super) fn held_back(&mut self, call_ref: &str) {
        if let Some(deferred) = self.calls.get_mut(call_ref) {
            deferred.arm_queued = false;
        }
    }

    /// The deferred arming of `call_ref` ran: the call is resident, or gone.
    pub(super) fn armed(&mut self, call_ref: &str) {
        let Some(deferred) = self.calls.remove(call_ref) else { return };
        for key in call::call_index_keys(&deferred.call) {
            if self.index.get(&key).is_some_and(|owner| owner == call_ref) {
                self.index.remove(&key);
            }
        }
    }

    /// Hold `event`, a copy of the initial INVITE of the deferred `call_ref`
    /// whose arming is not queued yet.
    pub(super) fn hold(&mut self, call_ref: &str, event: CallEvent) {
        if let Some(deferred) = self.calls.get_mut(call_ref) {
            deferred.held.push(event);
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
