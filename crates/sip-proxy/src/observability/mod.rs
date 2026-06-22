//! Observability for the proxy data path — [`metrics`] (counters/gauges +
//! Prometheus exposition). The `/metrics` + `/readyz` HTTP endpoint now lives in
//! the shared `probe-http` crate (one server for both the proxy and the b2bua
//! worker), wired in `sip-proxy-runner`. Routing decisions are observable through
//! `sip_routing_decision_total{kind}`; the old per-packet structured logger
//! seam was deleted (it was never wired — production always ran NoopLogger
//! while paying its per-packet record allocations).

pub mod metrics;
pub mod peer_failures;

pub use metrics::ProxyMetrics;
