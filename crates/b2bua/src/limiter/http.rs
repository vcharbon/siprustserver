//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the call-keyed limiter API through a [`BackendClient`] (ADR-0043):
//! the limiter's own client over an injected [`HttpTransport`] (real
//! `reqwest` in the runner, the simulated fabric in tests), its name looked
//! up through its [`ResolvedTarget`], at most [`MAX_IN_FLIGHT`] requests in
//! flight, and its breaker probing `GET /v1/health`. The **timeout budgets
//! live here**: every request is one attempt, never retried, under its
//! budget, and a timeout *or* any transport error (a target name that does
//! not resolve included) *or* the breaker open *or* a non-200 status maps to
//! `Unavailable`. An admit runs on a call's turn under the fail-open budget;
//! a refresh runs off every call, in the worker's refresh batch, and a
//! release off every call, in the worker's release queue, each under its own
//! longer budget, since no call waits on either. The client's health
//! ([`CallLimiter::health`]) is its breaker; each probe is counted as a
//! health request, and a probe's valid answer states the limiter's lease.
//! Every admit, refresh and health answer states the limiter's lease, which
//! the client reports to every [`LimiterReports`] registered with
//! [`CallLimiter::report_to`]; a lease is learnt only from a body judged
//! valid. A 200 whose body cannot be read (unknown shape, no lease or one
//! below `MIN_LEASE_MS`, a contradiction, a refresh answer not naming one
//! outcome per call) is a bad answer: handled as no answer. Every request,
//! with the call keys it names, and every request with no usable answer is
//! reported by its cause ([`LimiterFailure`]) and counted in the fail-open
//! episode under it; one that never left (the open breaker, no in-flight
//! slot within its budget) is reported as a failure only.

use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use call_limiter_wire::{
    self as wire, AdmitAnswer, AdmitEntry, AdmitRequest, AdmitResponse, HealthResponse, HeldSet,
    RefreshRequest, RefreshResponse, ReleaseRequest,
};
use http_net::{
    AttemptError, BackendClient, BreakerConfig, ClientSettings, HttpRequest, HttpResponse,
    HttpTransport, ResolvedTarget,
};
use tokio::sync::watch;

use crate::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHealth, LimiterHeld, LimiterReports,
    RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply, ReleaseAnswer,
};
use crate::metrics::{LimiterFailure, LimiterOp};

/// The shortest lease an answer may state: the limiter's own floor.
const MIN_LEASE_MS: u64 = 1_000;

/// The release budget a client runs with unless told otherwise.
const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// The refresh budget a client runs with unless told otherwise.
const DEFAULT_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

/// The limiter client's cap on requests in flight.
pub const MAX_IN_FLIGHT: usize = 64;

/// The path the limiter's breaker probes.
pub const HEALTH_PATH: &str = "/v1/health";

/// HTTP-backed limiter client over a pluggable transport.
pub struct HttpCallLimiter {
    endpoint: Arc<Endpoint>,
    /// The admit and health budget.
    timeout: Duration,
    refresh_timeout: Duration,
    release_timeout: Duration,
}

/// The limiter's client and every request of one client goes through it,
/// and the client's fail-open episode.
struct Endpoint {
    client: BackendClient,
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
    /// A 200 came back: the episode may end.
    fn answered(&self) {
        // One relaxed load while healthy.
        if self.fail_open.is_active() {
            self.fail_open.recovered(&self.addr_key);
        }
    }

    /// A request of `op` naming `keys` call keys left.
    fn sent(&self, op: LimiterOp, keys: usize) {
        for to in self.reports.read().unwrap_or_else(PoisonError::into_inner).iter() {
            to.sent(op, keys);
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

    /// Send `req` for `op`, naming `keys` call keys, under `budget`, once:
    /// the 200 answer, or `None` after counting why none came back. One that
    /// never left — kept by the open breaker, or refused an in-flight slot
    /// for its whole budget — is no request: it is counted as a failure only.
    async fn call(
        &self,
        op: LimiterOp,
        req: HttpRequest,
        keys: usize,
        budget: Duration,
    ) -> Option<HttpResponse> {
        let answer = self.client.attempt(req, budget).await;
        if !matches!(answer, Err(AttemptError::BreakerOpen | AttemptError::Saturated)) {
            self.sent(op, keys);
        }
        let cause = match answer {
            Ok(resp) if resp.status == 200 => return Some(resp),
            Ok(_) => LimiterFailure::Status,
            Err(AttemptError::Timeout | AttemptError::Saturated) => LimiterFailure::Timeout,
            Err(AttemptError::Transport(_)) => LimiterFailure::Transport,
            Err(AttemptError::BreakerOpen) => LimiterFailure::BreakerOpen,
        };
        self.failed(op, cause);
        None
    }

    /// One probe of the breaker's: a health request, its answer reported
    /// as an admit's is.
    fn probed(&self, answer: Result<&HttpResponse, &AttemptError>) {
        self.sent(LimiterOp::Health, 0);
        match answer {
            Err(AttemptError::Timeout) => self.failed(LimiterOp::Health, LimiterFailure::Timeout),
            Err(_) => self.failed(LimiterOp::Health, LimiterFailure::Transport),
            Ok(resp) if resp.status != 200 => {
                self.failed(LimiterOp::Health, LimiterFailure::Status);
            }
            Ok(resp) => match read_health(&resp.body) {
                Some(lease_ms) => {
                    self.answered();
                    self.stated_lease(lease_ms);
                }
                None => self.failed(LimiterOp::Health, LimiterFailure::BadAnswer),
            },
        }
    }
}

/// The limiter's breaker: its rule, its probe of [`HEALTH_PATH`] under the
/// admit budget `timeout`.
pub fn breaker_config(timeout: Duration) -> BreakerConfig {
    BreakerConfig { probe_timeout: timeout, ..BreakerConfig::probing(HEALTH_PATH) }
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the admit fail-open budget, `DEFAULT_REFRESH_TIMEOUT` as the refresh
    /// budget and `DEFAULT_RELEASE_TIMEOUT` as the release budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self::with_target(transport, ResolvedTarget::addr(addr), timeout)
    }

    /// [`new`](Self::new) for any [`ResolvedTarget`], under the default
    /// client settings ([`MAX_IN_FLIGHT`]) and [`breaker_config`].
    pub fn with_target(
        transport: Arc<dyn HttpTransport>,
        target: ResolvedTarget,
        timeout: Duration,
    ) -> Self {
        let client = BackendClient::new(
            transport,
            Arc::new(target),
            ClientSettings::with_max_in_flight(MAX_IN_FLIGHT),
            breaker_config(timeout),
        );
        Self::with_client(client, timeout)
    }

    /// [`new`](Self::new) over the limiter's own `client`.
    pub fn with_client(client: BackendClient, timeout: Duration) -> Self {
        let addr_key = client.target().to_string();
        let endpoint = Arc::new(Endpoint {
            client,
            fail_open: crate::lifecycle::backend_waves("call-limiter"),
            addr_key,
            reports: RwLock::default(),
        });
        let probes = Arc::downgrade(&endpoint);
        endpoint.client.report_probes_to(Arc::new(move |answer| {
            if let Some(endpoint) = probes.upgrade() {
                endpoint.probed(answer);
            }
        }));
        Self {
            endpoint,
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
        let target = self.endpoint.client.target();
        tokio::time::timeout(budget, target.resolve()).await.ok().flatten().is_some()
    }

    /// One request of `op` whose body serializes `body`, naming `keys` call
    /// keys, under `budget`; `None` when the limiter is unavailable (a
    /// timeout, a transport error, the breaker open, a non-200 answer).
    async fn post<T: serde::Serialize>(
        &self,
        op: LimiterOp,
        path: &str,
        body: &T,
        keys: usize,
        budget: Duration,
    ) -> Option<HttpResponse> {
        let bytes = serde_json::to_vec(body).ok()?;
        let resp = self.endpoint.call(op, HttpRequest::post(path, bytes), keys, budget).await?;
        // The episode ends once the limiter stops failing, so a limiter
        // answering every other request stays one episode.
        self.endpoint.answered();
        Some(resp)
    }
}

/// The client's health: its backend breaker.
struct HttpHealth {
    endpoint: Arc<Endpoint>,
}

impl LimiterHealth for HttpHealth {
    fn breaker(&self) -> watch::Receiver<bool> {
        self.endpoint.client.breaker_state()
    }

    fn has_address(&self) -> bool {
        self.endpoint.client.target().address().is_some()
    }

    fn trip(&self) {
        self.endpoint.client.trip();
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
        let Some(resp) = self.post(LimiterOp::Admit, "/v1/admit", &body, 1, self.timeout).await
        else {
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
        match self
            .post(LimiterOp::Release, "/v1/release", &body, keys.len(), self.release_timeout)
            .await
        {
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
        let Some(resp) = self
            .post(LimiterOp::Refresh, "/v1/refresh", &body, calls.len(), self.refresh_timeout)
            .await
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
        Some(Arc::new(HttpHealth { endpoint: self.endpoint.clone() }))
    }

    fn report_to(&self, reports: LimiterReports) {
        let mut all = self.endpoint.reports.write().unwrap_or_else(PoisonError::into_inner);
        all.retain(LimiterReports::is_live);
        all.push(reports);
    }
}

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
mod tests;
