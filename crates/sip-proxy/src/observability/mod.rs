//! Observability for the proxy data path — [`metrics`] (counters/gauges +
//! Prometheus exposition). The `/metrics` + `/readyz` HTTP endpoint lives in
//! the shared `probe-http` crate (one server for both the proxy and the b2bua
//! worker), wired in `sip-proxy-runner`. Routing decisions are observable through
//! `sip_routing_decision_total{kind}`. There is no per-packet logger seam; sampled
//! per-call tracing (`trace`, ADR-0026) is the only per-packet record.

pub mod catalogue;
pub mod metrics;
pub mod peer_failures;

pub use metrics::{ProxyMetrics, UdpShardStats};
