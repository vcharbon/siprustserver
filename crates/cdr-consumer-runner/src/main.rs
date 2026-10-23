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
//! Its link to the broker ([`link`](cdr_consumer_runner::link)) is set up by
//! one deadline and reconnected on any fault; `cdr_amqp_consuming` and
//! `cdr_amqp_links_ended_total` tell a broker out of reach, or a link that
//! keeps dropping, from an idle queue.
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
    let _observe = match observe::init_production("cdr-consumer-runner") {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("cdr-consumer-runner FATAL: {e}");
            std::process::exit(1);
        }
    };
    cdr_consumer_runner::app::run(Arc::new(JsonRecordView)).await;
}
