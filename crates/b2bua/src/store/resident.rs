//! Listings of the resident calls, read under one brief lock of the call map
//! without cloning a call: their `callRef`s, the active ones with their
//! answer, and the replication topologies they name.

use std::collections::HashSet;
use std::sync::Arc;

use call::{Call, CallModelState};

use super::CallState;

/// A resident `Active` call and the owners its topology names
/// ([`CallState::active_call_holders`]).
pub struct ActiveCall {
    pub call_ref: String,
    pub incarnation: String,
    /// `(primary, backup)`; `None` with no backup or no topology.
    pub owners: Option<(String, String)>,
}

impl CallState {
    /// Every callRef this node holds live. The core's teardown reads it to
    /// release the per-call runtime state that is NOT in the store — the root
    /// spans (ADR-0026) — and tests read it as ground truth.
    pub fn live_call_refs(&self) -> Vec<String> {
        self.locked().calls.keys().cloned().collect()
    }

    /// Every resident call still `Active`, as `(callRef, incarnation,
    /// answered)`: whether its caller's INVITE took a 2xx.
    pub fn active_call_answers(&self) -> Vec<(String, String, bool)> {
        let inner = self.locked();
        inner
            .calls
            .iter()
            .filter(|(_, call)| call.state == CallModelState::Active)
            .map(|(call_ref, call)| {
                let answered = call::helpers::caller_answered(call);
                (call_ref.clone(), call.incarnation().to_string(), answered)
            })
            .collect()
    }

    /// Every resident call still `Active`, with the `(primary, backup)` its
    /// topology names, `None` for a call with no backup (ADR-0055 §3) or
    /// naming no topology. The backing is read after the call map, as
    /// [`live_topologies`](Self::live_topologies) reads it; the caller weighs
    /// each holder after both.
    pub fn active_call_holders(&self) -> Vec<ActiveCall> {
        self.active_call_holders_with(|| ())
    }

    /// [`active_call_holders`](Self::active_call_holders), running `between`
    /// after the pass over the call map and before the backing read.
    fn active_call_holders_with(&self, between: impl FnOnce()) -> Vec<ActiveCall> {
        let mut holders: Vec<ActiveCall> = {
            let inner = self.locked();
            inner
                .calls
                .iter()
                .filter(|(_, call)| call.state == CallModelState::Active)
                .map(|(call_ref, call)| ActiveCall {
                    call_ref: call_ref.clone(),
                    incarnation: call.incarnation().to_string(),
                    owners: inner.topologies.get(call_ref).map(|t| (t.pri.clone(), t.bak.clone())),
                })
                .collect()
        };
        between();
        let unbacked = self.unbacked_refs();
        for holder in holders.iter_mut().filter(|h| unbacked.contains(&h.call_ref)) {
            holder.owners = None;
        }
        holders
    }

    /// The distinct `(primary, backup)` ordinal pairs the resident calls'
    /// topologies name; `None` when a resident call names no topology or has no
    /// backup (ADR-0055 §3): no peer can be holding it.
    pub fn live_topologies(&self) -> Option<Vec<(String, String)>> {
        self.live_topologies_with(|| ())
    }

    /// [`live_topologies`](Self::live_topologies), running `between` after the
    /// pass over the call map and before the backing read.
    fn live_topologies_with(&self, between: impl FnOnce()) -> Option<Vec<(String, String)>> {
        let (calls, pairs) = {
            let inner = self.locked();
            let mut pairs: Vec<(String, String)> = Vec::new();
            for call_ref in inner.calls.keys() {
                let t = inner.topologies.get(call_ref)?;
                if !pairs.iter().any(|(pri, bak)| *pri == t.pri && *bak == t.bak) {
                    pairs.push((t.pri.clone(), t.bak.clone()));
                }
            }
            let calls: Vec<Arc<Call>> = match self.repl_store() {
                Some(_) => inner.calls.values().cloned().collect(),
                None => Vec::new(),
            };
            (calls, pairs)
        };
        between();
        let unbacked = self.unbacked_refs();
        if calls.iter().any(|call| unbacked.contains(&call.call_ref)) {
            return None;
        }
        Some(pairs)
    }

    /// The calls with no backup, read after a pass over the call map: a new
    /// call is marked unbacked before it enters the map, so every call the
    /// pass saw already carries its mark (ADR-0055 §3).
    fn unbacked_refs(&self) -> HashSet<String> {
        self.repl_store().map(|repl| repl.unbacked_refs()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use call::Call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    use super::super::{CallStore, CallTopology, InMemoryCallStore};
    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use crate::metrics::B2buaMetrics;
    use crate::repl::ReplicatingCallStore;

    fn call(call_ref: &str) -> Call {
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-resident\r\n\
            Max-Forwards: 70\r\n\
            From: <sip:alice@example.com>;tag=alicetag\r\n\
            To: <sip:bob@example.com>\r\n\
            Call-ID: resident@10.0.0.9\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:alice@10.0.0.9:5060>\r\n\
            Content-Length: 0\r\n\r\n";
        let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
            panic!("expected a request")
        };
        let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let src = SocketAddr::from(([10, 0, 0, 9], 5060));
        let mut call = build_initial_call(&req, src, &cfg, &mut sip_ids::TurnIds::fixed(1), 0);
        call.call_ref = call_ref.into();
        call
    }

    /// Create the call `call_ref` placed on `topology`'s pair.
    fn create(state: &CallState, call_ref: &str, topology: Option<(&str, &str)>) {
        let topology = topology.map(|(pri, bak)| CallTopology::new(pri, bak, 0, 0));
        state.create(call(call_ref), topology);
    }

    fn state() -> CallState {
        let store: Arc<dyn CallStore> = Arc::new(InMemoryCallStore::new());
        CallState::new(store, "w0", B2buaMetrics::new())
    }

    #[test]
    fn each_topology_is_listed_once() {
        let state = state();
        create(&state, "a", Some(("w0", "w1")));
        create(&state, "b", Some(("w0", "w1")));
        create(&state, "c", Some(("w2", "w0")));
        let mut pairs = state.live_topologies().expect("every call names one");
        pairs.sort();
        assert_eq!(pairs, [("w0".into(), "w1".into()), ("w2".into(), "w0".into())]);
    }

    #[test]
    fn a_call_naming_no_topology_lists_none() {
        assert_eq!(state().live_topologies(), Some(Vec::new()), "no call, nothing named");
        let state = state();
        create(&state, "a", Some(("w0", "w1")));
        create(&state, "b", None);
        assert_eq!(state.live_topologies(), None);
    }

    /// A replicated state holding the backed `Active` call `a` on `(w0, w1)`.
    fn replicated() -> (CallState, Arc<ReplicatingCallStore>) {
        let repl = Arc::new(ReplicatingCallStore::new(1, sip_clock::Clock::test_at(0)));
        let state = state().with_replication(repl.clone());
        create_active(&state, "a", ("w0", "w1"));
        (state, repl)
    }

    /// Create the `Active` call `call_ref` placed on `(pri, bak)`.
    fn create_active(state: &CallState, call_ref: &str, (pri, bak): (&str, &str)) {
        let mut call = call(call_ref);
        call.state = CallModelState::Active;
        state.create(call, Some(CallTopology::new(pri, bak, 0, 0)));
    }

    /// The owners each listed holder names, by callRef.
    fn owners(holders: &[ActiveCall]) -> Vec<(&str, Option<(&str, &str)>)> {
        let mut owners: Vec<_> = holders
            .iter()
            .map(|h| {
                let pair = h.owners.as_ref().map(|(p, b)| (p.as_str(), b.as_str()));
                (h.call_ref.as_str(), pair)
            })
            .collect();
        owners.sort();
        owners
    }

    /// A call admitted unbacked after a listing's pass, its mark written
    /// before it enters the map, is not listed: the pass never saw it, and the
    /// next listing sees it with its mark. Red only against a backing read
    /// before the pass, which would list it backed.
    #[test]
    fn a_call_admitted_during_a_listing_is_never_listed_backed() {
        let admit = |state: &CallState, repl: &ReplicatingCallStore| {
            repl.leave_unbacked("c");
            create_active(state, "c", ("w0", "w2"));
        };
        let (state, repl) = replicated();
        let holders = state.active_call_holders_with(|| admit(&state, &repl));
        assert_eq!(owners(&holders), [("a", Some(("w0", "w1")))], "c is not listed");

        let (state, repl) = replicated();
        let pairs = state.live_topologies_with(|| admit(&state, &repl));
        assert_eq!(pairs, Some(vec![("w0".into(), "w1".into())]), "c is not listed");
        assert_eq!(state.live_topologies(), None, "the next listing sees c unbacked");
    }

    /// A resident call whose backup is given up between a listing's pass and
    /// its backing read is listed with no backup. The ordering's own red
    /// test: a backing read before the pass lists it backed.
    #[test]
    fn a_call_left_unbacked_during_a_listing_is_listed_unbacked() {
        let (state, repl) = replicated();
        create_active(&state, "b", ("w0", "w2"));
        let holders = state.active_call_holders_with(|| repl.leave_unbacked("b"));
        assert_eq!(owners(&holders), [("a", Some(("w0", "w1"))), ("b", None)]);

        let (state, repl) = replicated();
        create_active(&state, "b", ("w0", "w2"));
        assert_eq!(state.live_topologies_with(|| repl.leave_unbacked("b")), None);
        assert_eq!(state.live_topologies(), None, "and on every listing after it");
    }
}
