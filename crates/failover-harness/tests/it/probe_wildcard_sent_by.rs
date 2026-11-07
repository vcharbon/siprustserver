//! The proxy's health probe on a socket bound to the wildcard, as a runner binds
//! it: what it states names no address, and a real worker's answer finds it by
//! the `rport` it asks (RFC 3581 §4), so the worker reads alive.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{FailoverHarness, WorkerHealth};

const LB: &str = "127.0.0.1:5080";
const PROBE: &str = "0.0.0.0:49152";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

fn addr(at: &str) -> SocketAddr {
    at.parse().expect("an address")
}

#[tokio::test(start_paused = true)]
async fn a_worker_answering_a_probe_bound_to_the_wildcard_reads_alive() {
    let mut fh = FailoverHarness::new("probe-wildcard-sent-by", &["b1", "b2"]);
    let proxy =
        fh.spawn_proxy_with_health_probe(LB, PROBE, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    let (route, lb) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let b1 = fh.spawn_worker("b1", "b1", B1, &["b2"], route, lb).await;
    let b2 = fh.spawn_worker("b2", "b2", B2, &["b1"], route, lb).await;
    fh.advance(Duration::from_secs(3)).await;
    assert!(b1.is_ready() && b2.is_ready(), "both workers ready");
    assert_eq!(proxy.health("b1"), Some(WorkerHealth::Alive));
    assert_eq!(proxy.health("b2"), Some(WorkerHealth::Alive));
}
