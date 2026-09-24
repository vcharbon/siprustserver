//! The call limiter a [`B2buaSut`](crate::B2buaSut) runs against, and the
//! count the reaped check reads from it.
//!
//! By default the SUT reaches a real [`WindowStore`] served by a
//! [`LimiterServer`] on a private simulated HTTP fabric, through the production
//! [`HttpCallLimiter`], and every route its decision engine returns carries one
//! extra non-binding entry ([`DEFAULT_LIMITER_ID`]), so every routed call holds
//! a limiter and must drain it. Whatever limiter the SUT runs, a
//! [`HoldLedger`] counts the holds the SUT was granted and the holds it
//! released.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse,
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

/// The default client's fail-open budget. The default store answers every
/// request after the fabric's transit delay, so the budget only has to outlast
/// the largest single clock jump a paused scenario makes while a request is in
/// flight; a limiter that fails open is a scenario's own limiter.
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
    /// Holds the SUT released.
    pub released: i64,
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
}

impl HoldLedger {
    pub(crate) fn count(&self, store: Option<&WindowStore>) -> LimiterCount {
        LimiterCount {
            admitted: self.admitted.load(Ordering::SeqCst),
            released: self.released.load(Ordering::SeqCst),
            stored: store.map(|s| s.stats().current_total),
        }
    }
}

/// A [`CallLimiter`] that forwards to `inner` and records on `ledger` every
/// hold granted (on [`AdmitOutcome::Admitted`]) and every hold released (when
/// the release is issued). A refresh moves holds and counts nothing.
pub(crate) struct CountingLimiter {
    pub(crate) inner: Arc<dyn CallLimiter>,
    pub(crate) ledger: Arc<HoldLedger>,
}

#[async_trait]
impl CallLimiter for CountingLimiter {
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome {
        let outcome = self.inner.admit(entries).await;
        if matches!(outcome, AdmitOutcome::Admitted { .. }) {
            self.ledger.admitted.fetch_add(entries.len() as i64, Ordering::SeqCst);
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

/// The default limiter: a [`WindowStore`] on the harness clock, served on a
/// private simulated HTTP fabric and reached through [`HttpCallLimiter`].
pub(crate) struct DefaultLimiter {
    pub(crate) store: Arc<WindowStore>,
    pub(crate) client: Arc<dyn CallLimiter>,
    pub(crate) server: Box<dyn HttpServerHandle>,
}

impl DefaultLimiter {
    pub(crate) async fn serve() -> Self {
        let addr: SocketAddr = DEFAULT_LIMITER_ADDR.parse().expect("default limiter address");
        let http = SimulatedHttpNetwork::new();
        let cfg = LimiterConfig { ttl_sec: DEFAULT_STORE_TTL_SEC, ..LimiterConfig::default() };
        let store = Arc::new(WindowStore::new(cfg, Clock::test_at(0)));
        let service = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let server = http.serve(addr, service).await.expect("default limiter binds");
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(Arc::new(http), addr, DEFAULT_CLIENT_BUDGET));
        Self { store, client, server }
    }
}

/// A decision engine that appends the [`DEFAULT_LIMITER_ID`] entry to every
/// route `inner` returns (new call, failover, release reroute), after the
/// route's own entries, so both are admitted in one batch.
pub(crate) struct DefaultLimiterDecision {
    pub(crate) inner: Arc<dyn CallDecisionEngine>,
}

fn with_default_entry(treatment: CallTreatment) -> CallTreatment {
    match treatment {
        CallTreatment::Route(mut route) => {
            route
                .call_limiter
                .push(CallLimiterEntry { id: DEFAULT_LIMITER_ID.into(), limit: i64::MAX });
            CallTreatment::Route(route)
        }
        other => other,
    }
}

#[async_trait]
impl CallDecisionEngine for DefaultLimiterDecision {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await.map(with_default_entry)
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await.map(with_default_entry)
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
                match with_default_entry(CallTreatment::Route(route)) {
                    CallTreatment::Route(route) => Ok(CallReleaseResponse::Route(route)),
                    _ => unreachable!("a route stays a route"),
                }
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
        let count = LimiterCount { admitted: 1, released: 1, stored: Some(1) };
        count.assert_matches(LimiterLeak::NONE);
    }

    #[tokio::test]
    async fn only_routes_gain_the_default_entry() {
        use b2bua::decision::test_adapter::route_to;
        use b2bua::decision::RejectDecision;
        let mut own = route_to("127.0.0.1", 5070);
        own.call_limiter = vec![CallLimiterEntry { id: "x".into(), limit: 3 }];
        let CallTreatment::Route(route) = with_default_entry(CallTreatment::Route(own)) else {
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
        assert!(matches!(with_default_entry(reject), CallTreatment::Reject(_)));
    }
}
