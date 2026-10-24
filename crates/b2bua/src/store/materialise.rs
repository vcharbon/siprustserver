//! The one residency insert of a call served from a copy: a materialised
//! call enters the live map with its envelope (ADR-0014, ADR-0055 §7).

use std::sync::Arc;

use super::{CallState, Enveloped, MaterialiseOrigin};

impl CallState {
    /// The one residency insert: put a materialised call into the live map +
    /// routing index iff it is not already resident. Returns `true` when just
    /// inserted — the router then re-arms its timers exactly once. Idempotent: a
    /// call already live (re-served, or never lost) is left untouched.
    ///
    /// Every caller holds the call's per-call lock ([`lock`](Self::lock)), so no
    /// other insert of it runs between the residency check and the insert. A
    /// traced call opens this node's own root span here, after the residency
    /// check, so a span is opened only for a call this node goes on to serve
    /// (ADR-0026 §5). The idle clock starts at the insert, never at `created_at`
    /// (ADR-0020 X4): a hours-old failed-over or reclaimed long-hold call is
    /// fresh here, not reap-stale.
    ///
    /// The inserted call's limiter change counter moves to its next epoch
    /// (`CallLimiterState::enter_epoch`, ADR-0040 decision 8): the previous
    /// holder may have sent admits under numbers its last replicated write
    /// does not show.
    ///
    /// The call's envelope enters with it. On a [`MaterialiseOrigin::Reclaim`]
    /// insert it also re-establishes the replica's denormalised backup from
    /// the envelope's authoritative `bak` (ADR-0014 #4): the reboot-reclaim hydration imports the
    /// body peerless, leaving `CallMeta.backup == None` and the call invisible to
    /// its backup's bootstrap scan until the next keepalive re-flush. A takeover
    /// copy holds the `bak:` element itself and re-establishes nothing.
    pub fn materialize_if_absent(&self, copy: Enveloped, origin: MaterialiseOrigin) -> bool {
        let Enveloped { mut call, topology } = copy;
        let backup = topology.as_ref().map(|t| t.bak.clone()).filter(|b| !b.is_empty());
        let call_ref = call.call_ref.clone();
        let now_ms = self.clock.now_ms();
        if self.contains(&call_ref) {
            return false;
        }
        // Outside the store lock: the trace registry takes its own.
        crate::trace::adopt_replicated(&mut call, now_ms);
        call.limiter.enter_epoch();
        {
            let mut inner = self.locked();
            if inner.calls.contains_key(&call_ref) {
                return false;
            }
            Self::reindex(&mut inner, &call);
            inner.touched.insert(call_ref.clone(), now_ms);
            Self::place(&mut inner, &call_ref, topology);
            inner.restate_uncounted(false, call.limiter.runs_uncounted());
            self.publish_uncounted(&inner);
            inner.calls.insert(call_ref.clone(), Arc::new(call));
        }
        if origin == MaterialiseOrigin::Reclaim {
            if let (Some(repl), Some(backup)) = (self.repl_store(), backup) {
                repl.reestablish_backup(&call_ref, &backup);
            }
        }
        true
    }
}
