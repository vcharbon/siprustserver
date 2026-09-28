//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the call-keyed limiter API over an injected [`HttpTransport`] (real
//! `reqwest` in the runner, the simulated fabric in tests) to a
//! [`LimiterTarget`]. The **timeout budgets live here**: every request is
//! wrapped in `tokio::time::timeout`, and a timeout *or* any transport error
//! (a target name that does not resolve included) *or* a non-200 status maps
//! to `Unavailable`. An admit runs on a call's turn under the fail-open
//! budget; a refresh runs off every call, in the worker's refresh batch, and
//! a release off every call, in the worker's release queue, each under its
//! own longer budget, since no call waits on either. The health answer
//! ([`CallLimiter::health`]) asks `GET /v1/health`, which reads the limiter's
//! store, under the admit budget; it has an address once the target's name
//! resolved, and forgetting it makes the next request look the name up again.
//! Every admit and refresh answer states the limiter's lease, which the client
//! hands to every [`LimiterLease`] registered with
//! [`CallLimiter::report_lease`]; an answer without it is a bad body.

use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use call_limiter::wire::{
    self, AdmitEntry, AdmitRequest, AdmitResponse, HealthResponse, RefreshRequest, RefreshResponse,
    ReleaseRequest,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport};

use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, RefreshAnswer, RefreshCall,
    RefreshOutcome, ReleaseAnswer,
};
use crate::limiter_lease::LimiterLease;
use crate::limiter_target::LimiterTarget;

/// The release budget a client runs with unless told otherwise.
pub const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// The refresh budget a client runs with unless told otherwise.
pub const DEFAULT_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

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
    target: LimiterTarget,
    /// Fail-open aggregation keyed by the limiter target (ADR-0026): a limiter
    /// outage is ONE episode — rising edge, ~5 s summaries, falling-edge totals
    /// — ended once 200s have come back for the idle window. A failed health
    /// probe counts in it, so the episode lasts as long as the outage.
    fail_open: Arc<observe::WaveSet>,
    /// The target rendered once — the episode key, so an outage costs no
    /// per-request allocation.
    addr_key: String,
    /// Told every lease an answer states.
    leases: RwLock<Vec<Arc<LimiterLease>>>,
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

    /// One failure of kind `counter` in the episode.
    fn failed(&self, counter: &'static str) {
        self.fail_open.record(&self.addr_key, counter, 1);
    }

    /// An answer stated the limiter's lease, `lease_ms`.
    fn stated_lease(&self, lease_ms: u64) {
        let lease = Duration::from_millis(lease_ms);
        for to in self.leases.read().unwrap_or_else(PoisonError::into_inner).iter() {
            to.learn(lease);
        }
    }
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the admit fail-open budget, [`DEFAULT_REFRESH_TIMEOUT`] as the refresh
    /// budget and [`DEFAULT_RELEASE_TIMEOUT`] as the release budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self::with_target(transport, LimiterTarget::addr(addr), timeout)
    }

    /// [`new`](Self::new) for any [`LimiterTarget`].
    pub fn with_target(
        transport: Arc<dyn HttpTransport>,
        target: LimiterTarget,
        timeout: Duration,
    ) -> Self {
        let addr_key = target.to_string();
        Self {
            endpoint: Arc::new(Endpoint {
                transport,
                target,
                fail_open: crate::lifecycle::backend_waves("call-limiter"),
                addr_key,
                leases: RwLock::default(),
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

    /// Fire one request under `budget`. `None` on timeout / transport error /
    /// non-200 — the caller treats all three as "backend unavailable".
    async fn call(&self, req: HttpRequest, budget: Duration) -> Option<HttpResponse> {
        match tokio::time::timeout(budget, self.endpoint.send(req)).await {
            Ok(Ok(resp)) if resp.status == 200 => {
                // The episode ends once the limiter stops failing, so a
                // limiter answering every other request stays one episode.
                self.endpoint.answered();
                Some(resp)
            }
            other => {
                self.endpoint.failed(match other {
                    Err(_) => "timeouts",
                    Ok(Err(_)) => "transport_errors",
                    Ok(Ok(_)) => "non_200",
                });
                None
            }
        }
    }

    /// One request whose body serializes `body`, under `budget`; `None` when
    /// the limiter is unavailable.
    async fn post<T: serde::Serialize>(
        &self,
        path: &str,
        body: &T,
        budget: Duration,
    ) -> Option<HttpResponse> {
        let bytes = serde_json::to_vec(body).ok()?;
        self.call(HttpRequest::post(path, bytes), budget).await
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
        let answer =
            tokio::time::timeout(self.timeout, self.endpoint.send(HttpRequest::get("/v1/health")))
                .await;
        let serving = match answer {
            Ok(Ok(resp)) if resp.status == 200 => {
                serde_json::from_slice::<HealthResponse>(&resp.body).is_ok()
            }
            _ => false,
        };
        if serving {
            self.endpoint.answered();
        } else {
            self.endpoint.failed("probe");
            tracing::debug!(limiter = %self.endpoint.addr_key, "limiter health probe failed");
        }
        serving
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
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let body = AdmitRequest {
            key: key.to_string(),
            entries: entries
                .iter()
                .map(|e| AdmitEntry { id: e.id.clone(), limit: e.limit })
                .collect(),
            release_on_refusal,
        };
        let Some(resp) = self.post("/v1/admit", &body, self.timeout).await else {
            return AdmitOutcome::Unavailable;
        };
        let answer = serde_json::from_slice::<AdmitResponse>(&resp.body);
        if let Ok(AdmitResponse { lease_ms, .. }) = &answer {
            self.endpoint.stated_lease(*lease_ms);
        }
        match answer {
            Ok(AdmitResponse { admitted: true, .. }) => AdmitOutcome::Admitted,
            Ok(AdmitResponse { admitted: false, released: true, .. }) => AdmitOutcome::Released,
            Ok(AdmitResponse { admitted: false, rejected_id: Some(limiter_id), .. }) => {
                AdmitOutcome::Rejected { limiter_id }
            }
            // A malformed/contradictory body is treated as unavailable (fail-open).
            _ => AdmitOutcome::Unavailable,
        }
    }

    async fn release(&self, keys: &[String]) -> ReleaseAnswer {
        let body = ReleaseRequest { keys: keys.to_vec() };
        match self.post("/v1/release", &body, self.release_timeout).await {
            Some(_) => ReleaseAnswer::Released,
            None => ReleaseAnswer::Unavailable,
        }
    }

    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        let body = RefreshRequest {
            calls: calls
                .iter()
                .map(|c| wire::RefreshCall { key: c.key.clone(), ids: c.ids.clone() })
                .collect(),
        };
        let Some(resp) = self.post("/v1/refresh", &body, self.refresh_timeout).await else {
            return RefreshAnswer::Unavailable;
        };
        match serde_json::from_slice::<RefreshResponse>(&resp.body) {
            // An answer that does not name one outcome per call is a bad body.
            Ok(RefreshResponse { outcomes, lease_ms }) if outcomes.len() == calls.len() => {
                self.endpoint.stated_lease(lease_ms);
                RefreshAnswer::Answered(
                    outcomes
                        .into_iter()
                        .map(|outcome| match outcome {
                            wire::RefreshAnswer::Extended => RefreshOutcome::Extended,
                            wire::RefreshAnswer::Reregistered => RefreshOutcome::Reregistered,
                            wire::RefreshAnswer::Released => RefreshOutcome::Released,
                            wire::RefreshAnswer::Dropped => RefreshOutcome::Dropped,
                        })
                        .collect(),
                )
            }
            _ => RefreshAnswer::Unavailable,
        }
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        Some(Arc::new(HttpHealth { endpoint: self.endpoint.clone(), timeout: self.timeout }))
    }

    fn report_lease(&self, to: Arc<LimiterLease>) {
        self.endpoint.leases.write().unwrap_or_else(PoisonError::into_inner).push(to);
    }
}

#[cfg(test)]
mod tests {
    use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
    use http_net::{Fault, SimulatedHttpNetwork};
    use sip_clock::Clock;

    use super::*;

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
                tokio::spawn(async move { limiter.admit(&format!("c{n}#k"), &wide, false).await })
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
        let names = Arc::new(crate::limiter_target::tests::FakeResolver {
            delay: Some(Duration::from_millis(100)),
            ..Default::default()
        });
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        let target = LimiterTarget::name_with("limiter:8080", names.clone());
        let limiter: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
        assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn admits_during_a_lookup_slower_than_the_budget_fail_and_the_lookup_still_lands() {
        let (net, _server) = served().await;
        let names = Arc::new(crate::limiter_target::tests::FakeResolver {
            delay: Some(Duration::from_millis(300)),
            ..Default::default()
        });
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        let target = LimiterTarget::name_with("limiter:8080", names.clone());
        let limiter: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::with_target(Arc::new(net), target, BUDGET));
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Unavailable; 5]);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(five_at_once(&limiter).await, vec![AdmitOutcome::Admitted; 5]);
        assert_eq!(names.lookups.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn resolver() -> Arc<crate::limiter_target::tests::FakeResolver> {
        Arc::new(Default::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_does_not_resolve_fails_as_a_transport_error() {
        let (net, _server) = served().await;
        let target = LimiterTarget::name_with("limiter:8080", resolver());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Unavailable);
        let calls = [RefreshCall { key: "c#k".into(), ids: vec!["x".into()] }];
        assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);
        assert_eq!(client.release(&["c#k".into()]).await, ReleaseAnswer::Unavailable);
        assert!(!client.health().unwrap().serving().await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_named_client_has_an_address_once_its_name_resolves_until_it_forgets_it() {
        let (net, _server) = served().await;
        let names = resolver();
        let target = LimiterTarget::name_with("limiter:8080", names.clone());
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
            let body =
                RefreshResponse { outcomes: vec![wire::RefreshAnswer::Extended], lease_ms: 1_000 };
            HttpResponse::ok(serde_json::to_vec(&body).unwrap())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_refresh_request_answers_each_call_it_names() {
        let (net, _server) = served().await;
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        assert_eq!(client.admit("known#k", &entries(), false).await, AdmitOutcome::Admitted);
        client.release(&["gone#k".into()]).await;
        let call = |key: &str| RefreshCall { key: key.into(), ids: vec!["x".into()] };
        let answer = client.refresh(&[call("known#k"), call("lapsed#k"), call("gone#k")]).await;
        assert_eq!(
            answer,
            RefreshAnswer::Answered(vec![
                RefreshOutcome::Extended,
                RefreshOutcome::Reregistered,
                RefreshOutcome::Released,
            ])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_naming_fewer_outcomes_than_calls_is_unavailable() {
        let net = SimulatedHttpNetwork::new();
        let _server = net.serve(laddr(), Arc::new(OneOutcome)).await.unwrap();
        let client = HttpCallLimiter::new(Arc::new(net), laddr(), BUDGET);
        let call = |key: &str| RefreshCall { key: key.into(), ids: vec!["x".into()] };
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
        client.report_lease(a.clone());
        client.report_lease(b.clone());
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Admitted);
        assert_eq!([a.current(), b.current()], [Duration::from_secs(120); 2], "the store's lease");
        a.learn(Duration::from_secs(5));
        let calls = [RefreshCall { key: "c#k".into(), ids: vec!["x".into(), "y".into()] }];
        assert_eq!(
            client.refresh(&calls).await,
            RefreshAnswer::Answered(vec![RefreshOutcome::Extended])
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
        client.report_lease(lease.clone());
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Unavailable);
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
        client.report_lease(lease.clone());
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Unavailable);
        assert_eq!(lease.current(), Duration::from_secs(20), "nothing learnt");

        let (client, _server) = answering(r#"{"outcomes":["extended"],"lease_ms":0}"#).await;
        let calls = [RefreshCall { key: "c#k".into(), ids: vec!["x".into()] }];
        assert_eq!(client.refresh(&calls).await, RefreshAnswer::Unavailable);

        for body in [r#"{"calls":0,"lease_ms":999}"#, r#"{"calls":0}"#] {
            let (client, _server) = answering(body).await;
            assert!(!client.health().unwrap().serving().await, "{body}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_admit_answer_contradicting_itself_teaches_no_lease() {
        let (client, _server) =
            answering(r#"{"admitted":false,"released":false,"lease_ms":5000}"#).await;
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        client.report_lease(lease.clone());
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Unavailable);
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
        let target = LimiterTarget::name_with("limiter:8080", names.clone());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        assert!(!client.health().unwrap().serving().await, "not resolving yet");
        names.names.lock().unwrap().insert("limiter:8080".into(), laddr());
        assert!(client.health().unwrap().serving().await);
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Admitted);
    }
}
