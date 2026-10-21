//! The consumer's link: a setup the broker never finishes fails at its
//! deadline and shuts its socket down, a served one counts as consuming until
//! it drops. Real clock: the client's IO loop runs on a thread of its own.

use std::sync::Arc;
use std::time::Duration;

use cdr_consumer_runner::link::{LinkMetrics, Target};
use tokio::time::Instant;

use crate::support::broker::{Broker, Script};

const SETUP: Duration = Duration::from_millis(300);
/// Far above `SETUP`: a setup still running by then is unbounded.
const GIVE_UP: Duration = Duration::from_secs(5);

fn target(broker: &Broker) -> Target {
    Target { url: broker.url(), queue: "cdr".into(), max_len: 100, setup_timeout: SETUP }
}

fn line(metrics: &LinkMetrics, name: &str) -> String {
    let text = metrics.prometheus_text();
    text.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap_or_default().to_string()
}

/// Waits until the broker saw `want` connections ended by the client.
async fn ended_by_client(broker: &Broker, want: usize) -> usize {
    let give_up = Instant::now() + GIVE_UP;
    while broker.ended_by_client() < want && Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    broker.ended_by_client()
}

async fn a_stalled_setup_fails_at_its_deadline_and_ends_its_connection(script: Script) {
    let broker = Broker::start(script).await;
    let metrics = Arc::new(LinkMetrics::default());
    let started = Instant::now();
    let opened = tokio::time::timeout(GIVE_UP, target(&broker).open(&metrics))
        .await
        .expect("the setup must end by its own deadline");
    let error = opened.err().expect("a stalled setup fails").to_string();
    assert!(error.contains("deadline"), "{error}");
    assert!(started.elapsed() < SETUP * 3, "ended after {:?}", started.elapsed());
    assert_eq!(ended_by_client(&broker, 1).await, 1, "the abandoned setup must end its socket");
    assert_eq!(line(&metrics, "cdr_amqp_setup_failures_total"), "cdr_amqp_setup_failures_total 1");
    assert_eq!(line(&metrics, "cdr_amqp_consuming"), "cdr_amqp_consuming 0");
}

#[tokio::test]
async fn a_broker_silent_after_the_tcp_accept_fails_the_setup_at_its_deadline() {
    a_stalled_setup_fails_at_its_deadline_and_ends_its_connection(Script::StallHandshake).await;
}

#[tokio::test]
async fn a_queue_declare_never_answered_fails_the_setup_at_its_deadline() {
    a_stalled_setup_fails_at_its_deadline_and_ends_its_connection(Script::StallDeclare).await;
}

#[tokio::test]
async fn a_served_link_counts_as_consuming_until_it_drops_and_ends_its_connection() {
    let broker = Broker::start(Script::Serve).await;
    let metrics = Arc::new(LinkMetrics::default());
    let link = tokio::time::timeout(GIVE_UP, target(&broker).open(&metrics))
        .await
        .expect("bounded")
        .expect("a served setup links");
    assert_eq!(line(&metrics, "cdr_amqp_consuming"), "cdr_amqp_consuming 1");
    assert_eq!(broker.ended_by_client(), 0);
    drop(link);
    assert_eq!(line(&metrics, "cdr_amqp_consuming"), "cdr_amqp_consuming 0");
    assert_eq!(ended_by_client(&broker, 1).await, 1, "a dropped link must end its socket");
    assert_eq!(line(&metrics, "cdr_amqp_setup_failures_total"), "cdr_amqp_setup_failures_total 0");
}
