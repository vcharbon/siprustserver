//! The call limiter a [`B2buaSut`](crate::B2buaSut) runs against, and the
//! count the reaped check reads from it.
//!
//! By default the SUT reaches a real [`WindowStore`] served by a
//! [`LimiterServer`] on a private simulated HTTP fabric, through the production
//! [`HttpCallLimiter`], and every route its decision engine returns carries one
//! extra non-binding entry ([`DEFAULT_LIMITER_ID`]), so every routed call holds
//! a limiter and must drain it. Whatever limiter the SUT runs, a
//! [`HoldLedger`] counts the holds the SUT was granted, the holds it released
//! and the admits that failed open.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, RouteDecision,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold};
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;

/// The id of the entry the default limiter adds to every route. Its cap never
/// refuses, so it counts calls without changing any admission outcome.
pub const DEFAULT_LIMITER_ID: &str = "sut-default";

/// The default store's key TTL: longer than any scenario runs, so a leaked
/// hold is never swept away before the reaped check reads it.
const DEFAULT_STORE_TTL_SEC: i64 = 10 * 365 * 24 * 3600;

/// The default client's fail-open budget. Fail-open is never exercised on the
/// default limiter (the reaped check refuses one), so the budget only has to
/// outlast the largest single clock jump a paused scenario makes while a
/// request is in flight.
const DEFAULT_CLIENT_BUDGET: Duration = Duration::from_secs(24 * 3600);

/// Where the default limiter listens on its private fabric.
const DEFAULT_LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The limiter holds a scenario leaves behind on purpose, declared to
/// [`B2buaSut::assert_fully_reaped_leaving`](crate::B2buaSut::assert_fully_reaped_leaving).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimiterLeak {
    /// Holds the SUT was granted and never released.
    pub unreleased: i64,
    /// Holds the registered store still counts (a release the store never
    /// applied, an increment the SUT never learned of).
    pub stored: i64,
}

impl LimiterLeak {
    /// Nothing left behind: the reaped check's default.
    pub const NONE: Self = Self { unreleased: 0, stored: 0 };
}

/// The SUT's limiter count at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimiterCount {
    /// Holds the limiter granted the SUT (entries of every admitted batch).
    pub admitted: i64,
    /// Holds the SUT released, counted when the release is issued.
    pub released: i64,
    /// Admits that failed open ([`AdmitOutcome::Unavailable`]): the call went
    /// on holding nothing.
    pub failed_open: i64,
    /// The registered store's live count summed over every key, if the SUT
    /// has a store (the default limiter's, or one registered by the test).
    pub stored: Option<i64>,
}

impl LimiterCount {
    /// Holds granted and not released. Negative when the SUT released a hold
    /// it was never granted, or released one twice.
    pub fn unreleased(&self) -> i64 {
        self.admitted - self.released
    }

    /// Panics unless the count matches `leak`: `unreleased` exactly, and
    /// `stored` exactly when a store is registered.
    #[track_caller]
    pub fn assert_matches(&self, leak: LimiterLeak) {
        let unreleased = self.unreleased();
        assert!(
            unreleased >= 0,
            "limiter surplus: the SUT released {} hold(s) more than it was granted \
             ({} admitted, {} released)",
            -unreleased,
            self.admitted,
            self.released
        );
        assert_eq!(
            unreleased, leak.unreleased,
            "limiter leak: {unreleased} hold(s) granted and never released \
             ({} admitted, {} released); declared {}",
            self.admitted, self.released, leak.unreleased
        );
        if let Some(stored) = self.stored {
            assert_eq!(
                stored, leak.stored,
                "limiter leak: the store still counts {stored} hold(s); declared {}",
                leak.stored
            );
        }
    }
}

/// The holds a SUT was granted and released, counted at its limiter seam.
#[derive(Debug, Default)]
pub(crate) struct HoldLedger {
    admitted: AtomicI64,
    released: AtomicI64,
    failed_open: AtomicI64,
}

impl HoldLedger {
    pub(crate) fn count(&self, store: Option<&WindowStore>) -> LimiterCount {
        LimiterCount {
            admitted: self.admitted.load(Ordering::SeqCst),
            released: self.released.load(Ordering::SeqCst),
            failed_open: self.failed_open.load(Ordering::SeqCst),
            stored: store.map(|s| s.stats().current_total),
        }
    }
}

/// A [`CallLimiter`] that forwards to `inner` and records on `ledger` every
/// hold granted (on [`AdmitOutcome::Admitted`]), every admit that failed open
/// and every hold released (when the release is issued). A refresh moves
/// holds and counts nothing.
pub(crate) struct CountingLimiter {
    pub(crate) inner: Arc<dyn CallLimiter>,
    pub(crate) ledger: Arc<HoldLedger>,
}

#[async_trait]
impl CallLimiter for CountingLimiter {
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome {
        let outcome = self.inner.admit(entries).await;
        match outcome {
            AdmitOutcome::Admitted { .. } => {
                self.ledger.admitted.fetch_add(entries.len() as i64, Ordering::SeqCst);
            }
            AdmitOutcome::Unavailable => {
                self.ledger.failed_open.fetch_add(1, Ordering::SeqCst);
            }
            AdmitOutcome::Rejected { .. } => {}
        }
        outcome
    }

    async fn release(&self, holds: &[LimiterHold]) {
        self.ledger.released.fetch_add(holds.len() as i64, Ordering::SeqCst);
        self.inner.release(holds).await;
    }

    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        self.inner.refresh(holds).await
    }
}

/// An owned handle on a SUT's limiter count, for a check that outlives a
/// borrow of the SUT (a drop-time gate, a settle future).
#[derive(Clone)]
pub struct LimiterProbe {
    ledger: Arc<HoldLedger>,
    store: Option<Arc<WindowStore>>,
}

impl LimiterProbe {
    /// The count at this instant: what
    /// [`B2buaSut::limiter_count`](crate::B2buaSut::limiter_count) reads.
    pub fn count(&self) -> LimiterCount {
        self.ledger.count(self.store.as_deref())
    }
}

/// The limiter side of a running SUT: the ledger its limiter is counted on,
/// the store the reaped check reads, and the default limiter's server when the
/// SUT runs the default.
pub(crate) struct SutLimiter {
    ledger: Arc<HoldLedger>,
    store: Option<Arc<WindowStore>>,
    default_server: Option<Box<dyn HttpServerHandle>>,
}

impl SutLimiter {
    /// A test's own limiter, with the store behind it when registered.
    pub(crate) fn own(store: Option<Arc<WindowStore>>) -> Self {
        Self { ledger: Default::default(), store, default_server: None }
    }

    /// The default limiter: a [`WindowStore`] on the harness clock, served on
    /// a private simulated HTTP fabric. Returns the [`HttpCallLimiter`] that
    /// reaches it with the SUT side.
    pub(crate) async fn serve_default() -> (Arc<dyn CallLimiter>, Self) {
        let addr: SocketAddr = DEFAULT_LIMITER_ADDR.parse().expect("default limiter address");
        let http = SimulatedHttpNetwork::new();
        let cfg = LimiterConfig { ttl_sec: DEFAULT_STORE_TTL_SEC, ..LimiterConfig::default() };
        let store = Arc::new(WindowStore::new(cfg, Clock::test_at(0)));
        let service = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let server = http.serve(addr, service).await.expect("default limiter binds");
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(Arc::new(http), addr, DEFAULT_CLIENT_BUDGET));
        let sut =
            Self { ledger: Default::default(), store: Some(store), default_server: Some(server) };
        (client, sut)
    }

    pub(crate) fn ledger(&self) -> Arc<HoldLedger> {
        self.ledger.clone()
    }

    pub(crate) fn store(&self) -> Option<&Arc<WindowStore>> {
        self.store.as_ref()
    }

    pub(crate) fn count(&self) -> LimiterCount {
        self.ledger.count(self.store.as_deref())
    }

    pub(crate) fn probe(&self) -> LimiterProbe {
        LimiterProbe { ledger: self.ledger.clone(), store: self.store.clone() }
    }

    /// Check 6 of the reaped check: the count matches `leak`, and the default
    /// limiter never failed open (a fail-open would hide the call's holds).
    #[track_caller]
    pub(crate) fn assert_drained(&self, leak: LimiterLeak) {
        let count = self.count();
        if self.default_server.is_some() {
            assert_eq!(
                count.failed_open, 0,
                "the default limiter failed open on {} admit(s): those calls held nothing \
                 the reaped check can see",
                count.failed_open
            );
        }
        count.assert_matches(leak);
    }
}

/// A decision engine that appends the [`DEFAULT_LIMITER_ID`] entry to every
/// route `inner` returns (new call, failover, release reroute), after the
/// route's own entries, so both are admitted in one batch.
pub(crate) struct DefaultLimiterDecision {
    pub(crate) inner: Arc<dyn CallDecisionEngine>,
}

fn with_default_entry(mut route: RouteDecision) -> RouteDecision {
    route.call_limiter.push(CallLimiterEntry { id: DEFAULT_LIMITER_ID.into(), limit: i64::MAX });
    route
}

fn treatment_with_default_entry(treatment: CallTreatment) -> CallTreatment {
    match treatment {
        CallTreatment::Route(route) => CallTreatment::Route(with_default_entry(route)),
        other => other,
    }
}

#[async_trait]
impl CallDecisionEngine for DefaultLimiterDecision {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await.map(treatment_with_default_entry)
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await.map(treatment_with_default_entry)
    }

    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }

    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        match self.inner.call_release(req).await? {
            CallReleaseResponse::Route(route) => {
                Ok(CallReleaseResponse::Route(with_default_entry(route)))
            }
            release => Ok(release),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grants every batch at window 0 and ignores releases.
    struct Grants;

    #[async_trait]
    impl CallLimiter for Grants {
        async fn admit(&self, _entries: &[LimiterEntry]) -> AdmitOutcome {
            AdmitOutcome::Admitted { window: 0 }
        }
        async fn release(&self, _holds: &[LimiterHold]) {}
        async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
            holds.to_vec()
        }
    }

    /// Every admit fails open.
    struct FailsOpen;

    #[async_trait]
    impl CallLimiter for FailsOpen {
        async fn admit(&self, _entries: &[LimiterEntry]) -> AdmitOutcome {
            AdmitOutcome::Unavailable
        }
        async fn release(&self, _holds: &[LimiterHold]) {}
        async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
            holds.to_vec()
        }
    }

    /// One admit through `sut`'s ledger over a limiter that fails open.
    async fn one_fail_open(sut: &SutLimiter) {
        let limiter = CountingLimiter { inner: Arc::new(FailsOpen), ledger: sut.ledger() };
        limiter.admit(&[entry("x"), entry("y")]).await;
        assert_eq!(sut.count().failed_open, 1);
    }

    #[tokio::test]
    #[should_panic(expected = "the default limiter failed open on 1 admit(s)")]
    async fn a_fail_open_on_the_default_limiter_fails_the_check() {
        let (_client, sut) = SutLimiter::serve_default().await;
        one_fail_open(&sut).await;
        sut.assert_drained(LimiterLeak::NONE);
    }

    #[tokio::test]
    async fn a_fail_open_on_a_test_s_own_limiter_passes_the_check() {
        let sut = SutLimiter::own(None);
        one_fail_open(&sut).await;
        sut.assert_drained(LimiterLeak::NONE);
    }

    fn entry(id: &str) -> LimiterEntry {
        LimiterEntry { id: id.into(), limit: 10 }
    }

    fn hold(id: &str) -> LimiterHold {
        LimiterHold { limiter_id: id.into(), window: 0 }
    }

    fn counting() -> (CountingLimiter, Arc<HoldLedger>) {
        let ledger = Arc::new(HoldLedger::default());
        (CountingLimiter { inner: Arc::new(Grants), ledger: ledger.clone() }, ledger)
    }

    #[tokio::test]
    async fn a_probe_reads_the_count_after_its_sut_side_moves_on() {
        let (_client, sut) = SutLimiter::serve_default().await;
        let probe = sut.probe();
        let limiter = CountingLimiter { inner: Arc::new(Grants), ledger: sut.ledger() };
        limiter.admit(&[entry("x"), entry("y")]).await;
        limiter.release(&[hold("x")]).await;
        assert_eq!(probe.count(), sut.count());
        assert_eq!((probe.count().admitted, probe.count().released), (2, 1));
    }

    #[tokio::test]
    async fn every_granted_hold_released_once_matches_no_leak() {
        let (limiter, ledger) = counting();
        limiter.admit(&[entry("x"), entry("x"), entry("y")]).await;
        limiter.refresh(&[hold("x")]).await;
        limiter.release(&[hold("x"), hold("x"), hold("y")]).await;
        ledger.count(None).assert_matches(LimiterLeak::NONE);
    }

    #[tokio::test]
    #[should_panic(expected = "1 hold(s) granted and never released")]
    async fn a_granted_hold_never_released_is_a_leak() {
        let (limiter, ledger) = counting();
        limiter.admit(&[entry("x"), entry("y")]).await;
        limiter.release(&[hold("x")]).await;
        ledger.count(None).assert_matches(LimiterLeak::NONE);
    }

    #[tokio::test]
    #[should_panic(expected = "released 1 hold(s) more than it was granted")]
    async fn a_second_release_of_one_hold_is_a_surplus() {
        let (limiter, ledger) = counting();
        limiter.admit(&[entry("x")]).await;
        limiter.release(&[hold("x")]).await;
        limiter.release(&[hold("x")]).await;
        ledger.count(None).assert_matches(LimiterLeak::NONE);
    }

    #[tokio::test]
    async fn a_declared_unreleased_hold_matches() {
        let (limiter, ledger) = counting();
        limiter.admit(&[entry("x"), entry("y")]).await;
        ledger.count(None).assert_matches(LimiterLeak { unreleased: 2, stored: 0 });
    }

    #[test]
    #[should_panic(expected = "the store still counts 1 hold(s)")]
    fn a_hold_the_store_still_counts_is_a_leak() {
        let count = LimiterCount { admitted: 1, released: 1, failed_open: 0, stored: Some(1) };
        count.assert_matches(LimiterLeak::NONE);
    }

    #[tokio::test]
    async fn only_routes_gain_the_default_entry() {
        use b2bua::decision::test_adapter::route_to;
        use b2bua::decision::RejectDecision;
        let mut own = route_to("127.0.0.1", 5070);
        own.call_limiter = vec![CallLimiterEntry { id: "x".into(), limit: 3 }];
        let CallTreatment::Route(route) = treatment_with_default_entry(CallTreatment::Route(own))
        else {
            panic!("a route stays a route");
        };
        let ids: Vec<&str> = route.call_limiter.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["x", DEFAULT_LIMITER_ID], "the route's own entries come first");
        let reject = CallTreatment::Reject(RejectDecision {
            reject_code: 486,
            reject_reason: None,
            update_headers: None,
            service_ext: Default::default(),
            label: None,
        });
        assert!(matches!(treatment_with_default_entry(reject), CallTreatment::Reject(_)));
    }
}
