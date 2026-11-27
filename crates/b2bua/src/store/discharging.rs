//! The resident calls a stuck poll holds, being discharged before the process
//! exits (ADR-0057 §6): every delete written for one states the discharge, so
//! the peer holding its copy refuses the call's identity for
//! [`DISCHARGED_WINDOW`](crate::repl::DISCHARGED_WINDOW).

use super::CallState;

impl CallState {
    /// Mark the resident `call_ref` discharged: the deletes written for it
    /// from now on state it. A call not resident is not marked.
    pub fn mark_discharging(&self, call_ref: &str) {
        let mut inner = self.locked();
        if inner.calls.contains_key(call_ref) {
            inner.discharging.insert(call_ref.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use repl_net::frame::{Frame, Partition, Watermark};
    use sip_clock::Clock;

    use crate::metrics::B2buaMetrics;
    use crate::repl::ReplicatingCallStore;
    use crate::store::{CallState, CallStore, CallTopology, PartitionRole};

    /// A resident call `cid` of `w0`, backed up on `w1`; its `callRef`.
    fn resident(state: &CallState, cid: &str) -> String {
        let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        let invite = crate::test_call::invite("w0", "w1", cid);
        let mut ids = sip_ids::TurnIds::fixed(1);
        let src = crate::test_call::src();
        let call = crate::initial_invite::build_initial_call(&invite, src, &config, &mut ids, 0);
        state.create(call, Some(CallTopology::new("w0", "w1", 1, 0)))
    }

    /// Whether the delete of `call_ref` logged toward `w1` states a
    /// discharge; `None` when none is logged.
    async fn discharge_stated(repl: &ReplicatingCallStore, call_ref: &str) -> Option<bool> {
        let (since, role) = (Watermark::new(0, 0), PartitionRole::Primary);
        let frames =
            repl.changelog().drain_since("w1", Partition::Bak, since, 100, repl, role, "w0").await;
        frames.into_iter().find_map(|frame| match frame {
            Frame::Data { call_ref: of, discharged, .. } if of == call_ref => Some(discharged),
            _ => None,
        })
    }

    /// The delete of a call marked discharging states the discharge to its
    /// backup; the delete of a call that ended otherwise states none.
    #[tokio::test]
    async fn only_the_delete_of_a_call_being_discharged_states_it() {
        let repl = Arc::new(ReplicatingCallStore::new(1, Clock::test_at(0)));
        let store = repl.clone() as Arc<dyn CallStore>;
        let state = CallState::new(store, "w0", B2buaMetrics::new()).with_replication(repl.clone());
        let ended = resident(&state, "ended");
        let discharged = resident(&state, "discharged");
        state.mark_discharging(&discharged);
        state.remove(&ended);
        state.remove(&discharged);
        assert_eq!(discharge_stated(&repl, &ended).await, Some(false), "an ordinary end");
        assert_eq!(discharge_stated(&repl, &discharged).await, Some(true), "the discharge");
    }
}
