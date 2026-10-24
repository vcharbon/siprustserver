//! Listings of the resident calls, read under one brief lock of the call map
//! without cloning a call: their `callRef`s, the active ones with their
//! answer, and the replication topologies they name.

use call::CallModelState;

use super::CallState;

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

    /// The distinct `(primary, backup)` ordinal pairs the resident calls'
    /// topologies name; `None` when a resident call names no topology or has no
    /// backup (ADR-0055 §3): no peer can be holding it.
    pub fn live_topologies(&self) -> Option<Vec<(String, String)>> {
        let inner = self.locked();
        let mut pairs: Vec<(String, String)> = Vec::new();
        for call_ref in inner.calls.keys() {
            if self.repl_store().is_some_and(|repl| repl.unbacked(call_ref)) {
                return None;
            }
            let t = inner.topologies.get(call_ref)?;
            if !pairs.iter().any(|(pri, bak)| *pri == t.pri && *bak == t.bak) {
                pairs.push((t.pri.clone(), t.bak.clone()));
            }
        }
        Some(pairs)
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
}
