//! What the departure cells share: a two-worker cluster behind the front
//! proxy whose routing decision takes a known time to answer and counts every
//! consult, the kill the peers are told of, and the call a node placed read
//! back from its backup.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use b2bua::limiter::NoopLimiter;
use failover_harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut, WorkerHealth};

use crate::inflight_resend::KILL_SETTLE;

pub(crate) const PROXY: &str = "127.0.0.1:5080";
pub(crate) const B1: &str = "127.0.0.1:5091";
pub(crate) const B2: &str = "127.0.0.1:5092";

/// A decision engine that answers `new_call` after `delay`, counting the
/// consults it receives across every worker it serves.
pub(crate) struct CountedDecision {
    inner: Arc<dyn CallDecisionEngine>,
    delay: Duration,
    consults: Arc<AtomicUsize>,
}

impl CountedDecision {
    /// `inner`'s answers after `delay`, and the counter of consults.
    pub(crate) fn counting(
        inner: Arc<dyn CallDecisionEngine>,
        delay: Duration,
    ) -> (Arc<dyn CallDecisionEngine>, Arc<AtomicUsize>) {
        let consults = Arc::new(AtomicUsize::new(0));
        let engine = Arc::new(Self { inner, delay, consults: consults.clone() });
        (engine, consults)
    }
}

#[async_trait]
impl CallDecisionEngine for CountedDecision {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.consults.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// The proxy plus both workers on `decision`, ready at steady state; the
/// b-leg goes to the callee through the proxy.
pub(crate) async fn bring_up_deciding(
    fh: &mut FailoverHarness,
    decision: Arc<dyn CallDecisionEngine>,
) -> (ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let proxy = spawn_front_proxy(fh).await;
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let noop = || Arc::new(NoopLimiter);
    let b1 =
        fh.spawn_worker_limited("b1", "b1", B1, &["b2"], rt, ob, decision.clone(), noop()).await;
    let b2 = fh.spawn_worker_limited("b2", "b2", B2, &["b1"], rt, ob, decision, noop()).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready() && b2.is_ready(), "both workers ready at steady state");
    (proxy, b1, b2)
}

/// The front proxy over both workers.
pub(crate) async fn spawn_front_proxy(fh: &FailoverHarness) -> ProxySut {
    fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await
}

/// The call one worker placed, read back from the other's backup partition
/// once its first replica lands: the primary's ordinal and the `call_ref`.
pub(crate) async fn placed(
    fh: &FailoverHarness,
    b1: &ReplicatedB2buaSut,
    b2: &ReplicatedB2buaSut,
) -> (String, String) {
    for _ in 0..100 {
        if let Some(call_ref) = b2.scan_one_backed_up("b1").await {
            return ("b1".into(), call_ref);
        }
        if let Some(call_ref) = b1.scan_one_backed_up("b2").await {
            return ("b2".into(), call_ref);
        }
        fh.advance(Duration::from_millis(5)).await;
    }
    panic!("the call never reached its backup");
}

/// The kill the peers are told of: the node dies, the proxy's probe reads it
/// dead, and the orchestrator removes its endpoint, so the survivor parks it
/// and its departure sweep runs (ADR-0046). Returns the survivor's takeover
/// count before the kill.
pub(crate) async fn kill_announced(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) -> u64 {
    fh.mark(pri_ord, None, "crash", "serving node down; its endpoint leaves the membership");
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    primary.crash();
    proxy.set_health(pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(pri_ord);
    fh.advance(KILL_SETTLE).await;
    hydrated_before
}
