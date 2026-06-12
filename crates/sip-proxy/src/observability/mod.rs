//! Observability for the proxy data path — [`metrics`] (counters/gauges +
//! Prometheus exposition) and the `metrics_server` (the `/metrics` HTTP
//! endpoint). Routing decisions are observable through
//! `sip_routing_decision_total{kind}`; the old per-packet structured logger
//! seam was deleted (it was never wired — production always ran NoopLogger
//! while paying its per-packet record allocations).

pub mod metrics;
pub mod metrics_server;

pub use metrics::ProxyMetrics;
