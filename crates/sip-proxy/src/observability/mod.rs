//! Observability for the proxy data path — [`metrics`] (counters/gauges +
//! Prometheus exposition), the `logger` (structured routing-decision log), and
//! the `metrics_server` (the `/metrics` HTTP endpoint).

pub mod logger;
pub mod metrics;
pub mod metrics_server;

pub use metrics::ProxyMetrics;
