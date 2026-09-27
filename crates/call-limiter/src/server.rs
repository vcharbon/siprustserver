//! [`LimiterServer`] — the [`HttpService`] that routes the limiter API onto the
//! [`CallStore`], bumping [`LimiterMetrics`] at the edges.
//!
//! Routes: `POST /v1/admit`, `POST /v1/release`, `POST /v1/refresh`,
//! `GET /metrics`, `GET /healthz`. A malformed body is `400`; an unknown route
//! is `404`. The handler is pure compute (no real I/O), so the simulated fabric
//! drives it deterministically under a paused clock.

use std::sync::Arc;

use async_trait::async_trait;
use http_net::{HttpRequest, HttpResponse, HttpService};

use crate::metrics::LimiterMetrics;
use crate::store::{AdmitResult, CallStore, RefreshResult};
use crate::wire::{
    AdmitRequest, AdmitResponse, RefreshAnswer, RefreshRequest, RefreshResponse, ReleaseRequest,
};

/// The limiter HTTP service: a call store + its metrics.
pub struct LimiterServer {
    store: Arc<CallStore>,
    metrics: LimiterMetrics,
}

impl LimiterServer {
    /// Build over a shared store. The same store can be handed to the janitor.
    pub fn new(store: Arc<CallStore>, metrics: LimiterMetrics) -> Self {
        Self { store, metrics }
    }

    /// The shared store (for the runner's janitor task).
    pub fn store(&self) -> Arc<CallStore> {
        self.store.clone()
    }

    /// The metrics handle.
    pub fn metrics(&self) -> LimiterMetrics {
        self.metrics.clone()
    }
}

fn json_ok<T: serde::Serialize>(value: &T) -> HttpResponse {
    match serde_json::to_vec(value) {
        Ok(body) => HttpResponse::ok(body),
        Err(e) => HttpResponse::status(500).with_body(format!("serialize error: {e}").into_bytes()),
    }
}

fn bad_request(reason: &str) -> HttpResponse {
    HttpResponse::status(400).with_body(reason.as_bytes().to_vec())
}

#[async_trait]
impl HttpService for LimiterServer {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/v1/admit") => {
                let parsed: AdmitRequest = match serde_json::from_slice(&req.body) {
                    Ok(p) => p,
                    Err(e) => return bad_request(&format!("bad admit body: {e}")),
                };
                let outcome =
                    self.store.admit(&parsed.key, &parsed.entries, parsed.release_on_refusal);
                self.metrics.on_admit(&outcome);
                let resp = match outcome {
                    AdmitResult::Admitted => {
                        AdmitResponse { admitted: true, rejected_id: None, released: false }
                    }
                    AdmitResult::Rejected { limiter_id } => AdmitResponse {
                        admitted: false,
                        rejected_id: Some(limiter_id),
                        released: false,
                    },
                    AdmitResult::Released => {
                        AdmitResponse { admitted: false, rejected_id: None, released: true }
                    }
                };
                json_ok(&resp)
            }
            ("POST", "/v1/release") => {
                let parsed: ReleaseRequest = match serde_json::from_slice(&req.body) {
                    Ok(p) => p,
                    Err(e) => return bad_request(&format!("bad release body: {e}")),
                };
                self.store.release(&parsed.key);
                json_ok(&serde_json::json!({}))
            }
            ("POST", "/v1/refresh") => {
                let parsed: RefreshRequest = match serde_json::from_slice(&req.body) {
                    Ok(p) => p,
                    Err(e) => return bad_request(&format!("bad refresh body: {e}")),
                };
                let outcome = match self.store.refresh(&parsed.key, &parsed.ids) {
                    RefreshResult::Extended => RefreshAnswer::Extended,
                    RefreshResult::Reregistered => RefreshAnswer::Reregistered,
                    RefreshResult::Released => RefreshAnswer::Released,
                    RefreshResult::Dropped => RefreshAnswer::Dropped,
                };
                self.metrics.on_refresh();
                json_ok(&RefreshResponse { outcome })
            }
            ("GET", "/metrics") => {
                HttpResponse::ok(self.metrics.prometheus_text(self.store.stats()).into_bytes())
            }
            ("GET", "/healthz") => HttpResponse::ok(b"ok\n".to_vec()),
            _ => HttpResponse::status(404).with_body(b"not found\n".to_vec()),
        }
    }
}
