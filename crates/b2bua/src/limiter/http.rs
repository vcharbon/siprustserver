//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the call-keyed limiter API over an injected [`HttpTransport`] (real
//! `reqwest` in the runner, the simulated fabric in tests) to a
//! [`ResolvedTarget`]. The **timeout budgets live here**: every request is
//! wrapped in `tokio::time::timeout`, and a timeout *or* any transport error
//! (a target name that does not resolve included) *or* a non-200 status maps
//! to `Unavailable`. An admit runs on a call's turn under the fail-open
//! budget; a refresh runs off every call, in the worker's refresh batch, and
//! a release off every call, in the worker's release queue, each under its
//! own longer budget, since no call waits on either. The health answer
//! ([`CallLimiter::health`]) asks `GET /v1/health`, which reads the limiter's
//! store, under the admit budget; it has an address once the target's name
//! resolved, and forgetting it makes the next request look the name up again.
//! Every admit, refresh and health answer states the limiter's lease, which
//! the client reports to every [`LimiterReports`] registered with
//! [`CallLimiter::report_to`]; a lease is learnt only from a body judged
//! valid. A 200 whose body cannot be read (unknown shape, no lease or one
//! below `MIN_LEASE_MS`, a contradiction, a refresh answer not naming one
//! outcome per call) is a bad answer: handled as no answer. Every request and
//! every request with no usable answer is reported by its cause
//! ([`LimiterFailure`]) and counted in the fail-open episode under it.

use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use call_limiter::wire::{
    self, AdmitAnswer, AdmitEntry, AdmitRequest, AdmitResponse, HealthResponse, HeldSet,
    RefreshRequest, RefreshResponse, ReleaseRequest,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport};

use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer,
};
use crate::metrics::{LimiterFailure, LimiterOp};
use crate::resolved_target::ResolvedTarget;

/// The shortest lease an answer may state: the limiter's own floor.
const MIN_LEASE_MS: u64 = 1_000;

/// The release budget a client runs with unless told otherwise.
const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// The refresh budget a client runs with unless told otherwise.
const DEFAULT_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// HTTP-backed limiter client over a pluggable transport.
pub struct HttpCallLimiter {
    endpoint: Arc<Endpoint>,
    /// The admit and health budget.
    timeout: Duration,
    /// The refresh budget.
    refresh_timeout: Duration,
    /// The release budget.
    release_timeout: Duration,
}

/// A request never reached an answer: the transport failed, or the
/// target's name does not resolve.
struct TransportFailed;

/// The transport and the target every request of one client goes through,
/// and the client's fail-open episode.
struct Endpoint {
    transport: Arc<dyn HttpTransport>,
    target: ResolvedTarget,
    /// Fail-open aggregation keyed by the limiter target (ADR-0026): a limiter
    /// outage is ONE episode — rising edge, ~5 s summaries, falling-edge totals
    /// — ended once 200s have come back for the idle window. A failed health
    /// probe counts in it, so the episode lasts as long as the outage.
    fail_open: Arc<observe::WaveSet>,
    /// The target rendered once — the episode key, so an outage costs no
    /// per-request allocation.
    addr_key: String,
    /// Where the answers are reported.
    reports: RwLock<Vec<LimiterReports>>,
}

impl Endpoint {
    /// Send `req` to the target. `Err` on a transport error, a name that
    /// does not resolve included.
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportFailed> {
        let addr = self.target.resolve().await.ok_or(TransportFailed)?;
        self.transport.request(addr, req).await.map_err(|_| TransportFailed)
    }

    /// A 200 came back: the episode may end.
    fn answered(&self) {
        // One relaxed load while healthy.
        if self.fail_open.is_active() {
            self.fail_open.recovered(&self.addr_key);
        }
    }

    /// A request of `op` left.
    fn sent(&self, op: LimiterOp) {
        for to in self.reports.read().unwrap_or_else(PoisonError::into_inner).iter() {
            to.sent(op);
        }
    }

    /// A request of `op` got no usable answer for `cause`: one failure in the
    /// episode, reported.
    fn failed(&self, op: LimiterOp, cause: LimiterFailure) {
        self.fail_open.record(&self.addr_key, cause.label(), 1);
        for to in self.reports.read().unwrap_or_else(PoisonError::into_inner).iter() {
            to.failed(op, cause);
        }
    }

    /// A valid answer stated the limiter's lease, `lease_ms`.
    fn stated_lease(&self, lease_ms: u64) {
        let lease = Duration::from_millis(lease_ms);
        for to in self.reports.read().unwrap_or_else(PoisonError::into_inner).iter() {
            to.lease_stated(lease);
        }
    }

    /// Send `req` for `op` under `budget`: the 200 answer, or `None` after
    /// counting why none came back.
    async fn call(
        &self,
        op: LimiterOp,
        req: HttpRequest,
        budget: Duration,
    ) -> Option<HttpResponse> {
        self.sent(op);
        match tokio::time::timeout(budget, self.send(req)).await {
            Ok(Ok(resp)) if resp.status == 200 => Some(resp),
            other => {
                self.failed(
                    op,
                    match other {
                        Err(_) => LimiterFailure::Timeout,
                        Ok(Err(_)) => LimiterFailure::Transport,
                        Ok(Ok(_)) => LimiterFailure::Status,
                    },
                );
                None
            }
        }
    }
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the admit fail-open budget, `DEFAULT_REFRESH_TIMEOUT` as the refresh
    /// budget and `DEFAULT_RELEASE_TIMEOUT` as the release budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self::with_target(transport, ResolvedTarget::addr(addr), timeout)
    }

    /// [`new`](Self::new) for any [`ResolvedTarget`].
    pub fn with_target(
        transport: Arc<dyn HttpTransport>,
        target: ResolvedTarget,
        timeout: Duration,
    ) -> Self {
        let addr_key = target.to_string();
        Self {
            endpoint: Arc::new(Endpoint {
                transport,
                target,
                fail_open: crate::lifecycle::backend_waves("call-limiter"),
                addr_key,
                reports: RwLock::default(),
            }),
            timeout,
            refresh_timeout: DEFAULT_REFRESH_TIMEOUT,
            release_timeout: DEFAULT_RELEASE_TIMEOUT,
        }
    }

    /// The same client with `refresh_timeout` as its refresh budget.
    pub fn with_refresh_timeout(mut self, refresh_timeout: Duration) -> Self {
        self.refresh_timeout = refresh_timeout;
        self
    }

    /// The same client with `release_timeout` as its release budget.
    pub fn with_release_timeout(mut self, release_timeout: Duration) -> Self {
        self.release_timeout = release_timeout;
        self
    }

    /// Look the target's name up now, waiting at most `budget`: whether its
    /// address is known. A lookup still running past `budget` lands later
    /// and is kept.
    pub async fn lookup(&self, budget: Duration) -> bool {
        tokio::time::timeout(budget, self.endpoint.target.resolve()).await.ok().flatten().is_some()
    }

    /// One request of `op` whose body serializes `body`, under `budget`;
    /// `None` when the limiter is unavailable (a timeout, a transport error,
    /// a non-200 answer).
    async fn post<T: serde::Serialize>(
        &self,
        op: LimiterOp,
        path: &str,
        body: &T,
        budget: Duration,
    ) -> Option<HttpResponse> {
        let bytes = serde_json::to_vec(body).ok()?;
        let resp = self.endpoint.call(op, HttpRequest::post(path, bytes), budget).await?;
        // The episode ends once the limiter stops failing, so a limiter
        // answering every other request stays one episode.
        self.endpoint.answered();
        Some(resp)
    }
}

/// The client's health answer: `GET /v1/health` under the admit budget.
struct HttpHealth {
    endpoint: Arc<Endpoint>,
    timeout: Duration,
}

#[async_trait]
impl LimiterHealth for HttpHealth {
    async fn serving(&self) -> bool {
        let health = HttpRequest::get("/v1/health");
        let Some(resp) = self.endpoint.call(LimiterOp::Health, health, self.timeout).await else {
            tracing::debug!(limiter = %self.endpoint.addr_key, "limiter health probe failed");
            return false;
        };
        match read_health(&resp.body) {
            Some(lease_ms) => {
                self.endpoint.answered();
                self.endpoint.stated_lease(lease_ms);
                true
            }
            None => {
                self.endpoint.failed(LimiterOp::Health, LimiterFailure::BadAnswer);
                false
            }
        }
    }

    fn has_address(&self) -> bool {
        self.endpoint.target.address().is_some()
    }

    fn forget_address(&self) {
        self.endpoint.target.forget();
    }
}

#[async_trait]
impl CallLimiter for HttpCallLimiter {
    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let body = AdmitRequest {
            key: key.to_string(),
            change,
            held: HeldSet { change: held.change, entries: to_wire(&held.entries) },
            entries: to_wire(entries),
            release_on_refusal,
        };
        let Some(resp) = self.post(LimiterOp::Admit, "/v1/admit", &body, self.timeout).await else {
            return AdmitOutcome::Unavailable;
        };
        match read_admit(&resp.body) {
            Some((outcome, lease_ms)) => {
                self.endpoint.stated_lease(lease_ms);
                outcome
            }
            // A bad answer is no answer: the call fails open.
            None => {
                self.endpoint.failed(LimiterOp::Admit, LimiterFailure::BadAnswer);
                AdmitOutcome::Unavailable
            }
        }
    }

    fn admit_budget(&self) -> Duration {
        self.timeout
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        let body = ReleaseRequest { keys: keys.to_vec() };
        match self.post(LimiterOp::Release, "/v1/release", &body, self.release_timeout).await {
            Some(_) => ReleaseAnswer::Released,
            None => ReleaseAnswer::Unavailable,
        }
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        let body = RefreshRequest {
            calls: calls
                .iter()
                .map(|c| wire::RefreshCall {
                    key: c.key.clone(),
                    change: c.held.change,
                    entries: to_wire(&c.held.entries),
                })
                .collect(),
        };
        let Some(resp) =
            self.post(LimiterOp::Refresh, "/v1/refresh", &body, self.refresh_timeout).await
        else {
            return RefreshAnswer::Unavailable;
        };
        match read_refresh(&resp.body, calls.len()) {
            Some((outcomes, lease_ms)) => {
                self.endpoint.stated_lease(lease_ms);
                RefreshAnswer::Answered(outcomes)
            }
            None => {
                self.endpoint.failed(LimiterOp::Refresh, LimiterFailure::BadAnswer);
                RefreshAnswer::Unavailable
            }
        }
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        Some(Arc::new(HttpHealth { endpoint: self.endpoint.clone(), timeout: self.timeout }))
    }

    fn report_to(&self, reports: LimiterReports) {
        let mut all = self.endpoint.reports.write().unwrap_or_else(PoisonError::into_inner);
        all.retain(LimiterReports::is_live);
        all.push(reports);
    }
}

/// The wire form of `entries`.
fn to_wire(entries: &[LimiterEntry]) -> Vec<AdmitEntry> {
    entries.iter().map(|e| AdmitEntry { id: e.id.clone(), limit: e.limit }).collect()
}

/// The set a wire answer states.
fn from_wire(held: HeldSet) -> LimiterHeld {
    LimiterHeld {
        change: held.change,
        entries: held
            .entries
            .into_iter()
            .map(|e| LimiterEntry { id: e.id, limit: e.limit })
            .collect(),
    }
}

/// The outcome and the lease a valid admit answer states; `None` for a bad
/// answer (unreadable, no lease or one below [`MIN_LEASE_MS`], a cap refusal
/// naming no id, or an outcome but a fence's stating no held set).
fn read_admit(body: &[u8]) -> Option<(AdmitOutcome, u64)> {
    let AdmitResponse { outcome, rejected_id, held, lease_ms } =
        serde_json::from_slice(body).ok()?;
    let outcome = match (outcome, rejected_id, held) {
        (AdmitAnswer::Admitted, _, Some(_)) => AdmitOutcome::Admitted,
        (AdmitAnswer::Rejected, Some(limiter_id), Some(held)) => {
            AdmitOutcome::Rejected { limiter_id, held: from_wire(held) }
        }
        (AdmitAnswer::Superseded, _, Some(held)) => {
            AdmitOutcome::Superseded { held: from_wire(held) }
        }
        (AdmitAnswer::Released, _, _) => AdmitOutcome::Released,
        _ => return None,
    };
    (lease_ms >= MIN_LEASE_MS).then_some((outcome, lease_ms))
}

/// The replies and the lease a valid refresh answer for `calls` calls
/// states; `None` for a bad answer (unreadable, no lease or one below
/// [`MIN_LEASE_MS`], not one reply per call, or an outcome but `released`
/// stating no held set).
fn read_refresh(body: &[u8], calls: usize) -> Option<(Vec<RefreshReply>, u64)> {
    let RefreshResponse { outcomes, lease_ms } = serde_json::from_slice(body).ok()?;
    if outcomes.len() != calls || lease_ms < MIN_LEASE_MS {
        return None;
    }
    let replies = outcomes
        .into_iter()
        .map(|reply| {
            let outcome = match reply.outcome {
                wire::RefreshAnswer::Extended => RefreshOutcome::Extended,
                wire::RefreshAnswer::Reregistered => RefreshOutcome::Reregistered,
                wire::RefreshAnswer::Released => RefreshOutcome::Released,
                wire::RefreshAnswer::Dropped => RefreshOutcome::Dropped,
            };
            let held = reply.held.map(from_wire);
            (held.is_some() || outcome == RefreshOutcome::Released)
                .then_some(RefreshReply { outcome, held })
        })
        .collect::<Option<Vec<_>>>()?;
    Some((replies, lease_ms))
}

/// The lease a valid health answer states; `None` for a bad answer.
fn read_health(body: &[u8]) -> Option<u64> {
    let HealthResponse { lease_ms, .. } = serde_json::from_slice(body).ok()?;
    (lease_ms >= MIN_LEASE_MS).then_some(lease_ms)
}

#[cfg(test)]
mod tests {
    use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};

    use crate::limiter::lease::LimiterLease;
    use crate::metrics::B2buaMetrics;
    use http_net::{Fault, SimulatedHttpNetwork};
    use sip_clock::Clock;

    use super::*;
    use crate::limiter::testkit::held_of;

    const BUDGET: Duration = Duration::from_millis(150);

    fn laddr() -> SocketAddr {
        "10.0.0.1:8080".parse().unwrap()
    }

    fn entries() -> Vec<LimiterEntry> {
        vec![LimiterEntry { id: "x".into(), limit: 1 }, LimiterEntry { id: "y".into(), limit: 1 }]
    }

    async fn served() -> (SimulatedHttpNetwork, Box<dyn http_net::HttpServerHandle>) {
        let net = SimulatedHttpNetwork::new();
        let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(store, LimiterMetrics::new()));
        let handle = net.serve(laddr(), server).await.unwrap();
        (net, handle)
    }

    #[tokio::test(start_paused = true)]
    async fn the_health_answer_follows_the_limiter() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net.clone()), laddr(), BUDGET);
        let health = client.health().expect("the HTTP client has a health answer");
        assert!(health.serving().await);
        net.apply_fault(Fault::Stall { dst: laddr() });
        assert!(!health.serving().await, "a stalled limiter misses the budget");
        net.apply_fault(Fault::Cut { dst: laddr() });
        assert!(!health.serving().await);
        net.apply_fault(Fault::Resume { dst: laddr() });
        assert!(health.serving().await);
    }

    /// Five admits of distinct calls sent at once through `limiter`.
    async fn five_at_once(limiter: &Arc<dyn CallLimiter>) -> Vec<AdmitOutcome> {
        let admits: Vec<_> = (0..5)
            .map(|n| {
                let limiter = limiter.clone();
                let wide = vec![LimiterEntry { id: "x".into(), limit: 100 }];
                tokio::spawn(async move {
                    limiter
                        .admit(&format!("c{n}#k"), 1, &LimiterHeld::default(), &wide, false)
                        .await
                })
            })
            .collect();
        let mut outcomes = Vec::new();
        for admit in admits {
            outcomes.push(admit.await.unwrap());
        }
        outcomes
    }

    #[tokio::test(start_paused = true)]
    async fn admits_during_a_first_lookup_inside_the_budget_wait_for_it_and_are_admitted() {
        let (net, _server) = served().await;
        let names = Arc::new(crate::resolved_target::tests::FakeResolver {
            delay: Some(Duration::from_millis(100)),
            ..Default::default()
        });
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        let target = ResolvedTarget::name_with("limiter:8080", names.clone());
        let limiter: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
        assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn admits_during_a_lookup_slower_than_the_budget_fail_and_the_lookup_still_lands() {
        let (net, _server) = served().await;
        let names = Arc::new(crate::resolved_target::tests::FakeResolver {
            delay: Some(Duration::from_millis(300)),
            ..Default::default()
        });
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        let target = ResolvedTarget::name_with("limiter:8080", names.clone());
        let limiter: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Unavailable; 5]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
        assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn resolver() -> Arc<crate::resolved_target::tests::FakeResolver> {
        Arc::new(Default::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_does_not_resolve_fails_as_a_transport_error() {
        let (net, _server) = served().await;
        let target = ResolvedTarget::name_with("limiter:8080", resolver());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        assert_eq!(
            client.admit("c#k", 2, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Unavailable
        );
        let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }];
        assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);
        assert_eq!(client.release(&["c#k".into()]).await, ReleaseAnswer::Unavailable);
        assert!(!client.health().unwrap().serving().await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_client_has_an_address_once_its_name_resolves_until_it_forgets_it() {
        let (net, _server) = served().await;
        let names = resolver();
        let target = ResolvedTarget::name_with("limiter:8080", names.clone());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        let health = client.health().unwrap();
        assert!(!health.has_address(), "never looked up");
        assert!(!client.lookup(BUDGET).await, "does not resolve");
        assert!(!health.has_address());
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        assert!(client.lookup(BUDGET).await);
        assert!(health.has_address());
        health.forget_address();
        assert!(!health.has_address(), "forgotten");
        assert!(health.serving().await, "the probe looks it up again");
        assert!(health.has_address());
    }

    /// A limiter answering every refresh with one outcome whatever it names.
    struct OneOutcome;

    #[async_trait]
    impl http_net::HttpService for OneOutcome {
        async fn handle(&self, _: HttpRequest) -> HttpResponse {
            let reply = wire::RefreshReply {
                outcome: wire::RefreshAnswer::Extended,
                held: Some(HeldSet { change: 1, entries: vec![] }),
            };
            let body = RefreshResponse { outcomes: vec![reply], lease_ms: 1_000 };
            HttpResponse::ok(serde_json::to_vec(&body).unwrap())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_refresh_request_answers_each_call_it_names() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        assert_eq!(
            client.admit("known#k", 3, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Admitted
        );
        client.release(&["gone#k".into()]).await;
        let call = |key: &str| RefreshCall { key: key.into(), held: held_of(&["x".into()]) };
        let answer = client.refresh(&[call("known#k"), call("lapsed#k"), call("gone#k")]).await;
        let x = held_of(&["x".into()]);
        let RefreshAnswer::Answered(replies) = answer else { panic!("answered") };
        assert_eq!(
            replies,
            [
                RefreshReply {
                    outcome: RefreshOutcome::Extended,
                    held: Some(LimiterHeld { change: 3, entries: entries() }),
                },
                RefreshReply { outcome: RefreshOutcome::Reregistered, held: Some(x) },
                RefreshReply { outcome: RefreshOutcome::Released, held: None },
            ],
            "each reply states the set the limiter holds for its call"
        );
    }

    /// Every admit answer carries the set the limiter holds for the key: a
    /// cap refusal the set kept, an admit numbered at or below the held set's
    /// number the set held, untouched.
    #[tokio::test(start_paused = true)]
    async fn every_admit_answer_states_the_set_held() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let x = vec![LimiterEntry { id: "x".into(), limit: 1 }];
        let held = LimiterHeld { change: 2, entries: x.clone() };
        assert_eq!(
            client.admit("filler#k", 1, &LimiterHeld::default(), &entries()[1..], false).await,
            AdmitOutcome::Admitted
        );
        assert_eq!(
            client.admit("c#k", 2, &LimiterHeld::default(), &x, false).await,
            AdmitOutcome::Admitted
        );
        assert_eq!(
            client.admit("c#k", 1, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Superseded { held: held.clone() },
            "an older admit"
        );
        assert_eq!(
            client.admit("c#k", 3, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Rejected {
                limiter_id: "y".into(),
                held: LimiterHeld { change: 3, entries: x },
            },
            "the set kept, under the refusal's number"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_naming_fewer_outcomes_than_calls_is_unavailable() {
        let net = SimulatedHttpNetwork::new();
        let _server = net.serve(laddr(), Arc::new(OneOutcome)).await.unwrap();
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let call = |key: &str| RefreshCall { key: key.into(), held: held_of(&["x".into()]) };
        assert_eq!(client.refresh(&[call("a#k"), call("b#k")]).await, RefreshAnswer::Unavailable);
    }

    #[tokio::test(start_paused = true)]
    async fn every_admit_and_refresh_answer_hands_its_lease_to_every_registered_lease() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let (a, b) = (
            LimiterLease::starting_at(Duration::from_secs(20)),
            LimiterLease::starting_at(Duration::from_secs(30)),
        );
        client.report_to(LimiterReports::new(&a, B2buaMetrics::new()));
        client.report_to(LimiterReports::new(&b, B2buaMetrics::new()));
        assert_eq!(
            client.admit("c#k", 4, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Admitted
        );
        assert_eq!([a.current(), b.current()], [Duration::from_secs(120); 2], "the store's lease");
        a.learn(Duration::from_secs(5));
        let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into(), "y".into()]) }];
        assert_eq!(
            client.refresh(&calls).await,
            RefreshAnswer::Answered(vec![RefreshReply {
                outcome: RefreshOutcome::Extended,
                held: Some(LimiterHeld { change: 4, entries: entries() }),
            }])
        );
        assert_eq!(a.current(), Duration::from_secs(120), "a refresh answer states it too");
    }

    /// A limiter answering every admit with a body that states no lease.
    struct NoLease;

    #[async_trait]
    impl http_net::HttpService for NoLease {
        async fn handle(&self, _: HttpRequest) -> HttpResponse {
            HttpResponse::ok(br#"{"admitted":true,"released":false}"#.to_vec())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_admit_answer_stating_no_lease_is_unavailable() {
        let net = SimulatedHttpNetwork::new();
        let _server = net.serve(laddr(), Arc::new(NoLease)).await.unwrap();
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
        assert_eq!(
            client.admit("c#k", 5, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Unavailable
        );
        assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
    }

    /// A limiter answering every request with one fixed body.
    struct Answers(&'static str);

    #[async_trait]
    impl http_net::HttpService for Answers {
        async fn handle(&self, _: HttpRequest) -> HttpResponse {
            HttpResponse::ok(self.0.as_bytes().to_vec())
        }
    }

    async fn answering(
        body: &'static str,
    ) -> (HttpCallLimiter, Box<dyn http_net::HttpServerHandle>) {
        let net = SimulatedHttpNetwork::new();
        let server = net.serve(laddr(), Arc::new(Answers(body))).await.unwrap();
        (HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET), server)
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_below_one_second_is_a_bad_body() {
        let admitted = r#"{"admitted":true,"released":false,"lease_ms":999}"#;
        let (client, _server) = answering(admitted).await;
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        let metrics = B2buaMetrics::new();
        client.report_to(LimiterReports::new(&lease, metrics.clone()));
        assert_eq!(
            client.admit("c#k", 6, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Unavailable
        );
        assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
        let bad = |op| metrics.limiter().failures_total(op, LimiterFailure::BadAnswer);
        assert_eq!(bad(LimiterOp::Admit), 1, "a bad answer");

        let (client, _server) = answering(r#"{"outcomes":["extended"],"lease_ms":0}"#).await;
        client.report_to(LimiterReports::new(&lease, metrics.clone()));
        let calls = [RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }];
        assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);
        assert_eq!(bad(LimiterOp::Refresh), 1);

        for body in [r#"{"calls":0,"lease_ms":999}"#, r#"{"calls":0}"#] {
            let (client, _server) = answering(body).await;
            client.report_to(LimiterReports::new(&lease, metrics.clone()));
            assert!(!client.health().unwrap().serving().await, "{body}");
        }
        assert_eq!(bad(LimiterOp::Health), 2);
        assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");
    }

    /// Every request of `client`, one of each kind.
    async fn one_of_each(client: &HttpCallLimiter) {
        client.admit("c#k", 7, &LimiterHeld::default(), &entries(), false).await;
        client.refresh(&[RefreshCall { key: "c#k".into(), held: held_of(&["x".into()]) }]).await;
        client.release(&["c#k".into()]).await;
        client.health().unwrap().serving().await;
    }

    /// A limiter answering every request with `status`.
    struct Status(u16);

    #[async_trait]
    impl http_net::HttpService for Status {
        async fn handle(&self, _: HttpRequest) -> HttpResponse {
            HttpResponse::status(self.0)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_request_and_every_failure_is_counted_by_its_cause() {
        let ops = [LimiterOp::Admit, LimiterOp::Refresh, LimiterOp::Release, LimiterOp::Health];
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net.clone()), laddr(), BUDGET);
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        let metrics = B2buaMetrics::new();
        client.report_to(LimiterReports::new(&lease, metrics.clone()));
        let m = metrics.limiter();

        one_of_each(&client).await;
        for op in ops {
            assert_eq!((m.requests_total(op), m.failures_of(op)), (1, 0), "{op:?} answered");
        }
        net.apply_fault(Fault::Stall { dst: laddr() });
        one_of_each(&client).await;
        net.apply_fault(Fault::Cut { dst: laddr() });
        one_of_each(&client).await;
        for op in ops {
            assert_eq!(m.requests_total(op), 3, "{op:?}");
            assert_eq!(m.failures_total(op, LimiterFailure::Timeout), 1, "{op:?} stalled");
            assert_eq!(m.failures_total(op, LimiterFailure::Transport), 1, "{op:?} cut");
        }

        let net = SimulatedHttpNetwork::new();
        let _refusing = net.serve(laddr(), Arc::new(Status(503))).await.unwrap();
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        client.report_to(LimiterReports::new(&lease, metrics.clone()));
        one_of_each(&client).await;
        for op in ops {
            assert_eq!(m.failures_total(op, LimiterFailure::Status), 1, "{op:?} not 200");
            assert_eq!(m.failures_of(op), 3, "{op:?}");
        }
        assert_eq!(m.failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_health_answer_states_the_lease() {
        let (client, _server) = answering(r#"{"calls":0,"lease_ms":200000}"#).await;
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
        assert!(client.health().unwrap().serving().await);
        assert_eq!(lease.current(), Duration::from_secs(200));
    }

    #[tokio::test(start_paused = true)]
    async fn a_registration_whose_worker_is_gone_is_dropped() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        for _ in 0..3 {
            let gone = LimiterLease::starting_at(Duration::from_secs(20));
            client.report_to(LimiterReports::new(&gone, B2buaMetrics::new()));
        }
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
        assert_eq!(client.endpoint.reports.read().unwrap().len(), 1, "only the live worker");
        assert_eq!(
            client.admit("c#k", 8, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Admitted
        );
        assert_eq!(lease.current(), Duration::from_secs(120));
    }

    #[tokio::test(start_paused = true)]
    async fn an_admit_answer_contradicting_itself_teaches_no_lease() {
        let (client, _server) =
            answering(r#"{"admitted":false,"released":false,"lease_ms":5000}"#).await;
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_to(LimiterReports::new(&lease, B2buaMetrics::new()));
        assert_eq!(
            client.admit("c#k", 9, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Unavailable
        );
        assert_eq!(lease.current(), Duration::from_secs(20), "learnt only from a valid body");
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_at_an_address_always_has_it() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let health = client.health().unwrap();
        health.forget_address();
        assert!(health.has_address());
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_reaches_the_limiter_once_it_resolves() {
        let (net, _server) = served().await;
        let names = resolver();
        let target = ResolvedTarget::name_with("limiter:8080", names.clone());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        assert!(!client.health().unwrap().serving().await, "not resolving yet");
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        assert!(client.health().unwrap().serving().await);
        assert_eq!(
            client.admit("c#k", 10, &LimiterHeld::default(), &entries(), false).await,
            AdmitOutcome::Admitted
        );
    }
}
