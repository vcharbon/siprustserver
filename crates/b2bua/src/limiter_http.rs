//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the call-keyed limiter API over an injected [`HttpTransport`] (real
//! `reqwest` in the runner, the simulated fabric in tests) to a
//! [`LimiterTarget`]. The **timeout budgets live here**: every request is
//! wrapped in `tokio::time::timeout`, and a timeout *or* any transport error
//! (a target name that does not resolve included) *or* a non-200 status maps
//! to `Unavailable`. An admit or a refresh runs on a call's turn under the
//! fail-open budget; a release runs off every call, in the worker's release
//! queue, under its own longer budget. The health answer
//! ([`CallLimiter::health`]) asks `GET /v1/health`, which reads the limiter's
//! store, under the admit budget.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use call_limiter::wire::{
    AdmitEntry, AdmitRequest, AdmitResponse, HealthResponse, RefreshAnswer, RefreshRequest,
    RefreshResponse, ReleaseRequest,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport};

use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, RefreshOutcome, ReleaseAnswer,
};
use crate::limiter_target::LimiterTarget;

/// The release budget a client runs with unless told otherwise.
pub const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// HTTP-backed limiter client over a pluggable transport.
pub struct HttpCallLimiter {
    endpoint: Arc<Endpoint>,
    /// The admit, refresh and health budget.
    timeout: Duration,
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
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the admit and refresh fail-open budget and [`DEFAULT_RELEASE_TIMEOUT`]
    /// as the release budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self::with_target(transport, LimiterTarget::addr(addr), timeout)
    }

    /// [`new`](Self::new) for a limiter named `host:port`, resolved on the
    /// request path by the host's resolver ([`LimiterTarget::name`]).
    pub fn named(
        transport: Arc<dyn HttpTransport>,
        name: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self::with_target(transport, LimiterTarget::name(name), timeout)
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
            }),
            timeout,
            release_timeout: DEFAULT_RELEASE_TIMEOUT,
        }
    }

    /// The same client with `release_timeout` as its release budget.
    pub fn with_release_timeout(mut self, release_timeout: Duration) -> Self {
        self.release_timeout = release_timeout;
        self
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
        match serde_json::from_slice::<AdmitResponse>(&resp.body) {
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

    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
        let body = RefreshRequest { key: key.to_string(), ids: ids.to_vec() };
        let Some(resp) = self.post("/v1/refresh", &body, self.timeout).await else {
            return RefreshOutcome::Unavailable;
        };
        match serde_json::from_slice::<RefreshResponse>(&resp.body) {
            Ok(RefreshResponse { outcome: RefreshAnswer::Extended }) => RefreshOutcome::Extended,
            Ok(RefreshResponse { outcome: RefreshAnswer::Reregistered }) => {
                RefreshOutcome::Reregistered
            }
            Ok(RefreshResponse { outcome: RefreshAnswer::Released }) => RefreshOutcome::Released,
            Ok(RefreshResponse { outcome: RefreshAnswer::Dropped }) => RefreshOutcome::Dropped,
            Err(_) => RefreshOutcome::Unavailable,
        }
    }

    fn health(&self) -> Option<Arc<dyn LimiterHealth>> {
        Some(Arc::new(HttpHealth { endpoint: self.endpoint.clone(), timeout: self.timeout }))
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

    fn resolver() -> Arc<crate::limiter_target::tests::FakeResolver> {
        Arc::new(Default::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_name_that_does_not_resolve_fails_as_a_transport_error() {
        let (net, _server) = served().await;
        let target = LimiterTarget::name_with("limiter:8080", resolver());
        let client = HttpCallLimiter::with_target(Arc::new(net), target, BUDGET);
        assert_eq!(client.admit("c#k", &entries(), false).await, AdmitOutcome::Unavailable);
        assert_eq!(client.refresh("c#k", &["x".into()]).await, RefreshOutcome::Unavailable);
        assert_eq!(client.release(&["c#k".into()]).await, ReleaseAnswer::Unavailable);
        assert!(!client.health().unwrap().serving().await);
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
