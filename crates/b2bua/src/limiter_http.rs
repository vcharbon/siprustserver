//! [`HttpCallLimiter`] — the production limiter client.
//!
//! Speaks the batched/transactional limiter API over an injected
//! [`HttpTransport`] (real `reqwest` in the runner, the simulated fabric in
//! tests). The **fail-open timeout budget lives here**: every request is wrapped
//! in `tokio::time::timeout`, and a timeout *or* any transport error *or* a
//! non-200 status maps to [`AdmitOutcome::Unavailable`] — `apply_route` then
//! admits the call with no holds.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use call_limiter::wire::{
    AdmitEntry, AdmitRequest, AdmitResponse, Hold, RefreshRequest, RefreshResponse, ReleaseRequest,
};
use http_net::{HttpRequest, HttpResponse, HttpTransport};

use crate::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold};

/// HTTP-backed limiter client over a pluggable transport.
pub struct HttpCallLimiter {
    transport: Arc<dyn HttpTransport>,
    addr: SocketAddr,
    timeout: Duration,
}

impl HttpCallLimiter {
    /// Build a client targeting the limiter service at `addr`, with `timeout` as
    /// the per-request fail-open budget.
    pub fn new(transport: Arc<dyn HttpTransport>, addr: SocketAddr, timeout: Duration) -> Self {
        Self {
            transport,
            addr,
            timeout,
        }
    }

    /// Fire one request under the fail-open budget. `None` on timeout / transport
    /// error / non-200 — the caller treats all three as "backend unavailable".
    async fn call(&self, req: HttpRequest) -> Option<HttpResponse> {
        match tokio::time::timeout(self.timeout, self.transport.request(self.addr, req)).await {
            Ok(Ok(resp)) if resp.status == 200 => Some(resp),
            _ => None,
        }
    }
}

#[async_trait]
impl CallLimiter for HttpCallLimiter {
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome {
        let body = AdmitRequest {
            entries: entries
                .iter()
                .map(|e| AdmitEntry {
                    id: e.id.clone(),
                    limit: e.limit,
                })
                .collect(),
        };
        let bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(_) => return AdmitOutcome::Unavailable,
        };
        let Some(resp) = self.call(HttpRequest::post("/v1/admit", bytes)).await else {
            return AdmitOutcome::Unavailable;
        };
        match serde_json::from_slice::<AdmitResponse>(&resp.body) {
            Ok(AdmitResponse {
                admitted: true,
                window: Some(window),
                ..
            }) => AdmitOutcome::Admitted { window },
            Ok(AdmitResponse {
                admitted: false,
                rejected_id: Some(limiter_id),
                ..
            }) => AdmitOutcome::Rejected { limiter_id },
            // A malformed/contradictory body is treated as unavailable (fail-open).
            _ => AdmitOutcome::Unavailable,
        }
    }

    async fn release(&self, holds: &[LimiterHold]) {
        if holds.is_empty() {
            return;
        }
        let body = ReleaseRequest {
            entries: holds
                .iter()
                .map(|h| Hold {
                    id: h.limiter_id.clone(),
                    window: h.window,
                })
                .collect(),
        };
        if let Ok(bytes) = serde_json::to_vec(&body) {
            // Best-effort: a lost release self-heals via TTL + window rotation.
            let _ = self.call(HttpRequest::post("/v1/release", bytes)).await;
        }
    }

    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        if holds.is_empty() {
            return Vec::new();
        }
        let body = RefreshRequest {
            entries: holds
                .iter()
                .map(|h| Hold {
                    id: h.limiter_id.clone(),
                    window: h.window,
                })
                .collect(),
        };
        let bytes = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(_) => return holds.to_vec(),
        };
        match self.call(HttpRequest::post("/v1/refresh", bytes)).await {
            Some(resp) => match serde_json::from_slice::<RefreshResponse>(&resp.body) {
                Ok(RefreshResponse { entries }) => entries
                    .into_iter()
                    .map(|h| LimiterHold {
                        limiter_id: h.id,
                        window: h.window,
                    })
                    .collect(),
                // On a bad body, keep the old holds (no migration this cycle).
                Err(_) => holds.to_vec(),
            },
            None => holds.to_vec(),
        }
    }
}
