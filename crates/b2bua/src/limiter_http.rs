//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the call-keyed limiter API over an injected [`HttpTransport`] (real
//! `reqwest` in the runner, the simulated fabric in tests). The **fail-open
//! timeout budget lives here**: every request is wrapped in
//! `tokio::time::timeout`, and a timeout *or* any transport error *or* a
//! non-200 status maps to [`AdmitOutcome::Unavailable`] — the call site then
//! fails open, owing the call's release (the request may have landed).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use call_limiter::wire::{
    AdmitEntry, AdmitRequest, AdmitResponse, RefreshAnswer, RefreshRequest, RefreshResponse,
    ReleaseRequest,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport};

use crate::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, RefreshOutcome};

/// HTTP-backed limiter client over a pluggable transport.
pub struct HttpCallLimiter {
    transport: Arc<dyn HttpTransport>,
    addr: SocketAddr,
    timeout: Duration,
    /// Fail-open aggregation keyed by the limiter address (ADR-0026): a limiter
    /// outage is ONE episode — rising edge, ~5 s summaries, falling-edge totals
    /// — ended once 200s have come back for the idle window.
    fail_open: Arc<observe::WaveSet>,
    /// `addr` rendered once — the episode key, so an outage costs no
    /// per-request allocation.
    addr_key: String,
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the per-request fail-open budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self {
            transport,
            addr,
            timeout,
            fail_open: crate::lifecycle::backend_waves("call-limiter"),
            addr_key: addr.to_string(),
        }
    }

    /// Fire one request under the fail-open budget. `None` on timeout / transport
    /// error / non-200 — the caller treats all three as "backend unavailable".
    async fn call(&self, req: HttpRequest) -> Option<HttpResponse> {
        match tokio::time::timeout(self.timeout, self.transport.request(self.addr, req)).await {
            Ok(Ok(resp)) if resp.status == 200 => {
                // A 200 reports the recovery; the episode ends once the
                // limiter stops failing, so a limiter answering every other
                // request stays one episode. One relaxed load while healthy.
                if self.fail_open.is_active() {
                    self.fail_open.recovered(&self.addr_key);
                }
                Some(resp)
            }
            other => {
                let counter = match other {
                    Err(_) => "timeouts",
                    Ok(Err(_)) => "transport_errors",
                    Ok(Ok(_)) => "non_200",
                };
                self.fail_open.record(&self.addr_key, counter, 1);
                None
            }
        }
    }

    /// One request whose body serializes `body`; `None` when the limiter is
    /// unavailable.
    async fn post<T: serde::Serialize>(&self, path: &str, body: &T) -> Option<HttpResponse> {
        let bytes = serde_json::to_vec(body).ok()?;
        self.call(HttpRequest::post(path, bytes)).await
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
        let Some(resp) = self.post("/v1/admit", &body).await else {
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

    async fn release(&self, key: &str) {
        // Best-effort: a lost release lapses with the call's lease.
        let _ = self.post("/v1/release", &ReleaseRequest { key: key.to_string() }).await;
    }

    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
        let body = RefreshRequest { key: key.to_string(), ids: ids.to_vec() };
        let Some(resp) = self.post("/v1/refresh", &body).await else {
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
}
