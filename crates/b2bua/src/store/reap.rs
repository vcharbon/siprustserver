//! The replica store's reap: the expired bodies it evicts, sorted into the
//! deferred terminals and the ghosts the router ends.

use call::CallModelState;

use super::{envelope, CallState, Enveloped};

impl CallState {
    /// Physically evict every expired replica body, changelog tombstone and
    /// stale resurrection tombstone (the missed-delete ghost backstop,
    /// ADR-0014), and return the evicted bodies: the **deferred terminals**
    /// (`Terminating`/`Terminated`) — a node served the call's end and deferred
    /// its discharge to a primary that never reconciled it inside the TTL —
    /// and the **ghosts**, calls no node ended. The router releases each
    /// deferred terminal's limiter key and counts its CDR lost (the accepted
    /// double-failure), and ends a ghost whose primary departed with its CDR
    /// (ADR-0055 §5). A read never evicts, so each expired body is returned by
    /// exactly one pass. A deferred terminal another call on its ref replaced
    /// on this backup is returned the same way
    /// ([`take_displaced`](crate::repl::ReplicatingCallStore::take_displaced)).
    /// Empty without a replicating store.
    pub async fn reap_replica(&self, now_ms: i64) -> ReapedBodies {
        let Some(repl) = self.repl_store() else {
            return ReapedBodies::default();
        };
        let mut reaped = ReapedBodies::default();
        let decode = |b: &std::sync::Arc<[u8]>| envelope::decode(&self.codec, b).ok();
        for copy in repl.reap(now_ms).await.iter().filter_map(decode) {
            if ended(&copy) {
                reaped.deferred_terminals.push(copy);
            } else {
                reaped.ghosts.push(copy);
            }
        }
        let displaced = repl.take_displaced();
        reaped.deferred_terminals.extend(displaced.iter().filter_map(decode).filter(ended));
        reaped
    }
}

/// Whether `copy` is a call that ended or is ending.
fn ended(copy: &Enveloped) -> bool {
    matches!(copy.call.state, CallModelState::Terminating | CallModelState::Terminated)
}

/// The bodies one replica reap evicted ([`CallState::reap_replica`]).
#[derive(Default)]
pub struct ReapedBodies {
    /// Ended calls whose discharge their primary never reconciled.
    pub deferred_terminals: Vec<Enveloped>,
    /// Calls no node ended.
    pub ghosts: Vec<Enveloped>,
}
