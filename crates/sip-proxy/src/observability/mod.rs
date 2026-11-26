//! Observability for the proxy data path: [`metrics`] (counters, gauges and
//! their Prometheus exposition). The `/metrics` and `/readyz` endpoint is the
//! shared `probe-http` server, wired in `sip-proxy-runner`. Routing decisions
//! count in `sip_routing_decision_total{kind}`; the only per-packet record is
//! sampled per-call tracing (`crate::trace`, ADR-0026).

pub mod catalogue;
pub mod metrics;
pub mod peer_failures;

pub use metrics::{ProxyMetrics, UdpShardStats};
