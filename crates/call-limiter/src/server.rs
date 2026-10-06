//! [`LimiterServer`] — the [`HttpService`] that routes the limiter API onto the
//! [`CallStore`], bumping [`LimiterMetrics`] at the edges.
//!
//! Routes: `POST /v1/admit`, `POST /v1/release`, `POST /v1/refresh`,
//! `GET /v1/health`, `GET /metrics`, `GET /healthz`. Every admit, refresh and
//! health answer states the store's lease, and every admit and refresh answer
//! the set held for its key. `/healthz` answers the
//! process; `/v1/health` answers only once the store has, so a client's
//! breaker probing it learns that a request can be served. A malformed body
//! is `400`; an unknown route is `404`. The handler is pure compute (no real
//! I/O), so the simulated fabric drives it deterministically under a paused
//! clock.

use std::sync::Arc;

use async_trait::async_trait;
use http_net::{HttpRequest, HttpResponse, HttpService};

use crate::metrics::LimiterMetrics;
use crate::store::{AdmitResult, CallStore, RefreshResult};
use crate::wire::{
    AdmitAnswer, AdmitRequest, AdmitResponse, HealthResponse, HeldSet, RefreshAnswer, RefreshReply,
    RefreshRequest, RefreshResponse, ReleaseRequest,
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

    /// The lease every admit, refresh and health answer states.
    fn lease_ms(&self) -> u64 {
        self.store.lease_ms().max(0) as u64
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
                let AdmitRequest { key, change, held, entries, release_on_refusal } = parsed;
                let outcome =
                    self.store.admit_carrying(&key, change, &held, &entries, release_on_refusal);
                self.metrics.on_admit(&outcome);
                let (outcome, rejected_id, held) = match outcome {
                    AdmitResult::Admitted => {
                        (AdmitAnswer::Admitted, None, Some(HeldSet { change, entries }))
                    }
                    AdmitResult::Rejected { limiter_id, held } => {
                        (AdmitAnswer::Rejected, Some(limiter_id), Some(held))
                    }
                    AdmitResult::Superseded { held } => (AdmitAnswer::Superseded, None, Some(held)),
                    AdmitResult::Released => (AdmitAnswer::Released, None, None),
                };
                json_ok(&AdmitResponse { outcome, rejected_id, held, lease_ms: self.lease_ms() })
            }
            ("POST", "/v1/release") => {
                let parsed: ReleaseRequest = match serde_json::from_slice(&req.body) {
                    Ok(p) => p,
                    Err(e) => return bad_request(&format!("bad release body: {e}")),
                };
                self.store.release(&parsed.keys);
                self.metrics.on_release(parsed.keys.len());
                json_ok(&serde_json::json!({}))
            }
            ("POST", "/v1/refresh") => {
                let parsed: RefreshRequest = match serde_json::from_slice(&req.body) {
                    Ok(p) => p,
                    Err(e) => return bad_request(&format!("bad refresh body: {e}")),
                };
                let calls =
                    parsed.calls.iter().map(|c| (c.key.as_str(), c.change, c.entries.as_slice()));
                let results = self.store.refresh_all(calls);
                self.metrics.on_refresh(results.iter().map(|r| &r.result));
                let outcomes = results
                    .into_iter()
                    .map(|refreshed| RefreshReply {
                        outcome: match refreshed.result {
                            RefreshResult::Extended => RefreshAnswer::Extended,
                            RefreshResult::Reregistered => RefreshAnswer::Reregistered,
                            RefreshResult::Released => RefreshAnswer::Released,
                            RefreshResult::Dropped => RefreshAnswer::Dropped,
                        },
                        held: refreshed.held,
                    })
                    .collect();
                json_ok(&RefreshResponse { outcomes, lease_ms: self.lease_ms() })
            }
            ("GET", "/v1/health") => json_ok(&HealthResponse {
                calls: self.store.calls() as u64,
                lease_ms: self.lease_ms(),
            }),
            ("GET", "/metrics") => {
                HttpResponse::ok(self.metrics.prometheus_text(self.store.stats()).into_bytes())
            }
            ("GET", "/healthz") => HttpResponse::ok(b"ok\n".to_vec()),
            _ => HttpResponse::status(404).with_body(b"not found\n".to_vec()),
        }
    }
}
