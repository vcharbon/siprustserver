//! `cdr-consumer-runner` — the dedicated CDR metrics consumer.
//!
//! Drains the RabbitMQ CDR queue the b2bua workers publish to (one record per
//! terminated call) and turns it into Prometheus counters
//! ([`counting`](cdr_consumer_runner::counting)):
//!
//! - `cdr_consumed_total` — total number of CDRs consumed
//! - `cdr_call_duration_ms_total` — summed call duration across all CDRs,
//!   i.e. Σ (terminated_at − created_at), 0 in `opaque` mode — and
//!   `cdr_parse_errors_total` for payloads that do not decode.
//!
//! With `CDR_OBSERVATION_URL` set it also ships one `cdr-observation.v1` line
//! per base JSON record to VictoriaLogs for the HA tooling's CDR join
//! ([`shipper`](cdr_consumer_runner::shipper)). It persists nothing: on
//! restart the counters reset to 0 (handled by `rate()`/`increase()`).
//! Configuration: [`app`](cdr_consumer_runner::app).

use std::sync::Arc;

use cdr_consumer_runner::observation::JsonRecordView;

#[tokio::main]
async fn main() {
    // Subscriber first (ADR-0026); the guard drains the log writer at exit.
    let _observe = observe::init_production("cdr-consumer-runner");
    cdr_consumer_runner::app::run(Arc::new(JsonRecordView)).await;
}
