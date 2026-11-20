//! The call limiter a [`B2buaSut`](crate::B2buaSut) runs against, and the
//! count the reaped check reads from it.
//!
//! By default the SUT reaches a real [`CallStore`] served by a
//! [`LimiterServer`] on a private simulated HTTP fabric, through the production
//! [`HttpCallLimiter`], and every route its decision engine returns carries one
//! extra non-binding entry ([`DEFAULT_LIMITER_ID`]), so every routed call holds
//! a limiter and must drain it. Whatever limiter the SUT runs, a
//! `HoldLedger` counts, per call, the holds the SUT was granted, the holds
//! it released (a replaced set counts as released, and so does a set granted
//! to an admit the call's release overtook) and the admits that failed open; a drain
//! failure names each set left ([`UnreleasedSet`]). The limiter a test drives
//! itself is the [`WitnessRig`](crate::witness::WitnessRig); scripted
//! limiters are in [`doubles`].

pub mod doubles;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, CallTreatment,
    NewCallRequest, NewCallResponse, RouteDecision,
};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpServerHandle, HttpTransport};
use http_net_testkit::SimulatedHttpNetwork;
use sip_clock::Clock;

/// The id of the entry the default limiter adds to every route. Its cap never
/// refuses, so it counts calls without changing any admission outcome.
pub const DEFAULT_LIMITER_ID: &str = "sut-default";

/// The default store's lease: longer than any scenario runs, so a leaked
/// set never lapses before the reaped check reads it.
const DEFAULT_STORE_LEASE_SEC: i64 = 10 * 365 * 24 * 3600;

/// The default client's admit, refresh and release budget. Fail-open is never
/// exercised on the default limiter (the reaped check refuses one), so the
/// budget only has to outlast the largest single clock jump a paused scenario
/// makes while a request is in flight.
const DEFAULT_CLIENT_BUDGET: Duration = Duration::from_secs(24 * 3600);

/// Where the default limiter listens on its private fabric.
const DEFAULT_LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The limiter holds a scenario leaves behind on purpose, declared to
/// [`B2buaSut::assert_fully_reaped_leaving`](crate::B2buaSut::assert_fully_reaped_leaving).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LimiterLeak {
    /// Holds the SUT was granted and never released.
    pub unreleased: i64,
    /// Holds the registered store still counts (a release the store never
    /// applied, an increment the SUT never learned of).
    pub stored: i64,
    /// The limiter keys whose release still waits in the worker's release
    /// queue (a limiter that never answers them before the scenario ends).
    pub queued: Vec<String>,
}

impl LimiterLeak {
    /// Nothing left behind: the reaped check's default.
    pub(crate) const NONE: Self = Self { unreleased: 0, stored: 0, queued: Vec::new() };

    /// `queued` in a stable order, for comparison with the waiting keys.
    pub(crate) fn queued_sorted(&self) -> Vec<String> {
        let mut keys = self.queued.clone();
        keys.sort();
        keys
    }
}

/// The SUT's limiter count at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimiterCount {
    /// Holds the limiter granted the SUT (entries of every admitted set).
    pub admitted: i64,
    /// Holds the SUT released: the set of every call it released, and every
    /// set a later admit of the call replaced or a refusal dropped.
    pub released: i64,
    /// Admits the SUT ran the call past without a limiter answer: a lost
    /// answer ([`AdmitOutcome::Unavailable`]) and an admit that sent no
    /// request ([`AdmitOutcome::NotSent`], e.g. no limiter configured). The
    /// call went on as it was; its release frees whatever a request that
    /// landed left on the store. An admit the worker's open circuit breaker
    /// answered never reaches this seam: the worker counts it
    /// (`b2bua_limiter_failures_total{cause="breaker_open"}`).
    pub failed_open: i64,
    /// The registered store's live count summed over every id, if the SUT
    /// has a store (the default limiter's, or one registered by the test).
    pub stored: Option<i64>,
}

impl LimiterCount {
    /// Holds granted and not released. A release of a call the SUT holds no
    /// set for (a second release, a call never admitted) counts nothing, so
    /// this never reads negative.
    pub fn unreleased(&self) -> i64 {
        self.admitted - self.released
    }

    /// Panics unless the count matches `leak`: `unreleased` exactly, and
    /// `stored` exactly when a store is registered.
    #[track_caller]
    pub fn assert_matches(&self, leak: &LimiterLeak) {
        self.assert_matches_naming(leak, &[]);
    }

    /// [`assert_matches`](Self::assert_matches), a failure on `unreleased`
    /// naming each of `sets`.
    #[track_caller]
    fn assert_matches_naming(&self, leak: &LimiterLeak, sets: &[UnreleasedSet]) {
        let unreleased = self.unreleased();
        let named: String = sets.iter().map(|set| format!("\n  {set}")).collect();
        assert_eq!(
            unreleased, leak.unreleased,
            "limiter leak: {unreleased} hold(s) granted and never released \
             ({} admitted, {} released); declared {}{named}",
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

/// One set the SUT was granted and has not released, as a drain failure
/// names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreleasedSet {
    /// The call's limiter key: its incarnation, which carries its Call-ID.
    pub key: String,
    /// The holds of the set.
    pub holds: i64,
    /// What the registered store holds for the key.
    pub stored: StoreView,
}

/// What the registered store holds for one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreView {
    /// No store is registered.
    NoStore,
    /// The store released the key: it holds nothing and fences it.
    Released,
    /// The store holds a set of this many entries for the key (none for a
    /// key it knows nothing of).
    Holds(usize),
}

impl StoreView {
    fn of(store: Option<&CallStore>, key: &str) -> Self {
        match store.map(|s| s.held_set(key)) {
            None => Self::NoStore,
            Some(None) => Self::Released,
            Some(Some(set)) => Self::Holds(set.entries.len()),
        }
    }
}

impl std::fmt::Display for UnreleasedSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} hold(s)", self.key, self.holds)?;
        match self.stored {
            StoreView::NoStore => Ok(()),
            StoreView::Released => write!(f, ", the store released the key"),
            StoreView::Holds(entries) => write!(f, ", the store holds {entries}"),
        }
    }
}

/// The holds a SUT was granted and released, counted at its limiter seam:
/// per call, the size of the set the SUT holds.
#[derive(Debug, Default)]
pub(crate) struct HoldLedger {
    inner: Mutex<Ledger>,
}

#[derive(Debug, Default)]
struct Ledger {
    /// Limiter key -> holds of the set the SUT was granted and still holds.
    sets: HashMap<String, i64>,
    /// The keys a release was sent for. The limiter fences a released key
    /// against every later admit, so an admit sent before the key's release
    /// left and answered `Admitted` after it landed ahead of the release,
    /// which drops its set.
    released_keys: HashSet<String>,
    admitted: i64,
    released: i64,
    failed_open: i64,
}

impl HoldLedger {
    fn count(&self, store: Option<&CallStore>) -> LimiterCount {
        let ledger = self.inner.lock().unwrap();
        LimiterCount {
            admitted: ledger.admitted,
            released: ledger.released,
            failed_open: ledger.failed_open,
            stored: store.map(|s| s.stats().current_total),
        }
    }

    /// Every set granted and not released, by key, with what `store` holds
    /// for its key.
    fn unreleased(&self, store: Option<&CallStore>) -> Vec<UnreleasedSet> {
        let ledger = self.inner.lock().unwrap();
        let mut sets: Vec<UnreleasedSet> = ledger
            .sets
            .iter()
            .map(|(key, holds)| UnreleasedSet {
                key: key.clone(),
                holds: *holds,
                stored: StoreView::of(store, key),
            })
            .collect();
        sets.sort_by(|a, b| a.key.cmp(&b.key));
        sets
    }

    /// Whether a release of `key` was sent.
    fn is_released(&self, key: &str) -> bool {
        self.inner.lock().unwrap().released_keys.contains(key)
    }

    /// Record the answer to an admit of `key` asking for `granted` holds;
    /// `released_at_send`: a release of `key` had left when the admit was
    /// sent.
    fn on_admit(
        &self,
        key: &str,
        granted: i64,
        outcome: &AdmitOutcome,
        release_on_refusal: bool,
        released_at_send: bool,
    ) {
        let mut ledger = self.inner.lock().unwrap();
        match outcome {
            AdmitOutcome::Admitted => {
                ledger.released += ledger.sets.remove(key).unwrap_or(0);
                ledger.admitted += granted;
                if !released_at_send && ledger.released_keys.contains(key) {
                    ledger.released += granted;
                } else if granted > 0 {
                    ledger.sets.insert(key.to_string(), granted);
                }
            }
            AdmitOutcome::Rejected { .. } => {
                if release_on_refusal {
                    ledger.released += ledger.sets.remove(key).unwrap_or(0);
                }
            }
            AdmitOutcome::Released | AdmitOutcome::Superseded { .. } => {}
            AdmitOutcome::Unavailable | AdmitOutcome::NotSent => ledger.failed_open += 1,
        }
    }

    fn on_release(&self, key: &str) {
        let mut ledger = self.inner.lock().unwrap();
        ledger.released += ledger.sets.remove(key).unwrap_or(0);
        ledger.released_keys.insert(key.to_string());
    }
}

/// A [`CallLimiter`] that forwards to `inner` and records on `ledger` every
/// answered admit: each hold granted (on [`AdmitOutcome::Admitted`]), each
/// admit `inner` answered `Unavailable` or `NotSent` (failed open), and every
/// hold released: at a release of the call, at the replacement of its set,
/// at a refusal that dropped it, and at once for a set granted to an admit a
/// release of the call overtook in flight (the release drops it). That last
/// rule holds for an `inner` that fences a released key, as the
/// [`CallStore`] does. An admit `inner` never answers, cut off by the
/// worker's admit bound, records nothing. A refresh counts nothing.
pub(crate) struct CountingLimiter {
    pub(crate) inner: Arc<dyn CallLimiter>,
    pub(crate) ledger: Arc<HoldLedger>,
}

#[async_trait]
impl CallLimiter for CountingLimiter {
    fn admit_budget(&self) -> std::time::Duration {
        self.inner.admit_budget()
    }

    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let released_at_send = self.ledger.is_released(key);
        let outcome = self.inner.admit(key, change, held, entries, release_on_refusal).await;
        let granted = entries.len() as i64;
        self.ledger.on_admit(key, granted, &outcome, release_on_refusal, released_at_send);
        outcome
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        for key in keys {
            self.ledger.on_release(key);
        }
        self.inner.release(keys).await
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.inner.refresh(calls).await
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        self.inner.health()
    }

    fn report_to(&self, reports: LimiterReports) {
        self.inner.report_to(reports);
    }
}

/// An owned handle on a SUT's limiter count, for a check that outlives a
/// borrow of the SUT (a drop-time gate, a settle future).
#[derive(Clone)]
pub struct LimiterProbe {
    ledger: Arc<HoldLedger>,
    store: Option<Arc<CallStore>>,
}

impl LimiterProbe {
    /// The count at this instant: what
    /// [`B2buaSut::limiter_count`](crate::B2buaSut::limiter_count) reads.
    pub fn count(&self) -> LimiterCount {
        self.ledger.count(self.store.as_deref())
    }

    /// Every set granted and not released at this instant, by key.
    pub fn unreleased(&self) -> Vec<UnreleasedSet> {
        self.ledger.unreleased(self.store.as_deref())
    }

    /// Panics unless the count matches `leak`
    /// ([`LimiterCount::assert_matches`]), a failure naming each set
    /// granted and not released.
    #[track_caller]
    pub fn assert_matches(&self, leak: &LimiterLeak) {
        self.count().assert_matches_naming(leak, &self.unreleased());
    }
}

/// The limiter side of a running SUT: the ledger its limiter is counted on,
/// the store the reaped check reads, and the default limiter's server when the
/// SUT runs the default.
pub(crate) struct SutLimiter {
    ledger: Arc<HoldLedger>,
    store: Option<Arc<CallStore>>,
    default_server: Option<Box<dyn HttpServerHandle>>,
}

impl SutLimiter {
    /// A test's own limiter, with the store behind it when registered.
    pub(crate) fn own(store: Option<Arc<CallStore>>) -> Self {
        Self { ledger: Default::default(), store, default_server: None }
    }

    /// The default limiter: a [`CallStore`] on the harness clock, served on
    /// a private simulated HTTP fabric. Returns the [`HttpCallLimiter`] that
    /// reaches it with the SUT side.
    pub(crate) async fn serve_default() -> (Arc<dyn CallLimiter>, Self) {
        let addr: SocketAddr = DEFAULT_LIMITER_ADDR.parse().expect("default limiter address");
        let http = SimulatedHttpNetwork::new();
        let cfg = LimiterConfig { lease_sec: DEFAULT_STORE_LEASE_SEC };
        let store = Arc::new(CallStore::new(cfg, Clock::test_at(0)));
        let service = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let server = http.serve(addr, service).await.expect("default limiter binds");
        let client: Arc<dyn CallLimiter> = Arc::new(
            HttpCallLimiter::new(Arc::new(http), addr, DEFAULT_CLIENT_BUDGET)
                .with_refresh_timeout(DEFAULT_CLIENT_BUDGET)
                .with_release_timeout(DEFAULT_CLIENT_BUDGET),
        );
        let sut =
            Self { ledger: Default::default(), store: Some(store), default_server: Some(server) };
        (client, sut)
    }

    pub(crate) fn ledger(&self) -> Arc<HoldLedger> {
        self.ledger.clone()
    }

    pub(crate) fn store(&self) -> Option<&Arc<CallStore>> {
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
    pub(crate) fn assert_drained(&self, leak: &LimiterLeak) {
        let count = self.count();
        if self.default_server.is_some() {
            assert_eq!(
                count.failed_open, 0,
                "the default limiter failed open on {} admit(s): those calls held nothing \
                 the reaped check can see",
                count.failed_open
            );
        }
        count.assert_matches_naming(leak, &self.ledger.unreleased(self.store.as_deref()));
    }
}

/// A decision engine that appends the [`DEFAULT_LIMITER_ID`] entry to every
/// route `inner` returns (new call, failover, release reroute), after the
/// route's own entries, so both are admitted in one set.
pub(crate) struct DefaultLimiterDecision {
    pub(crate) inner: Arc<dyn CallDecisionEngine>,
}

fn with_default_entry(mut route: RouteDecision) -> RouteDecision {
    route.call_limiter.push(LimiterEntry { id: DEFAULT_LIMITER_ID.into(), limit: i64::MAX });
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
mod tests;
