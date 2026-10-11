use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;

/// A name lookup that never returns, as a resolver whose server never answers.
#[derive(Default)]
struct SilentResolver {
    lookups: AtomicUsize,
}

#[async_trait]
impl NameResolver for SilentResolver {
    async fn resolve(&self, _name: &str) -> Vec<SocketAddr> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

/// Resolve `peer` through `resolver`, returning the answer and the time it
/// took, or `None` when it is still waiting after a minute.
async fn timed(resolver: &ReplResolver, peer: &Peer) -> Option<(Option<SocketAddr>, Duration)> {
    let started = tokio::time::Instant::now();
    let answer =
        tokio::time::timeout(Duration::from_secs(60), resolver.resolve(peer)).await.ok()?;
    Some((answer, started.elapsed()))
}

#[tokio::test(start_paused = true)]
async fn a_lookup_that_never_returns_is_bounded_at_two_seconds() {
    let names = Arc::new(SilentResolver::default());
    let resolver = ReplResolver::new(9100, ReplAddressing::Static, names.clone());
    let answer = timed(&resolver, &Peer::new("w1", "peer-w1.example")).await;
    assert_eq!(
        answer,
        Some((None, Duration::from_secs(2))),
        "a lookup with no answer gives up after 2 s and the attempt reads unresolvable"
    );
    assert_eq!(names.lookups.load(Ordering::SeqCst), 1, "the name was looked up once");
}

#[tokio::test(start_paused = true)]
async fn a_discovered_peer_falls_back_to_its_pod_ip_once_its_name_lookup_is_out_of_time() {
    let names = Arc::new(SilentResolver::default());
    let addressing = ReplAddressing::PodDns { service: "workers".into(), namespace: "lab".into() };
    let resolver = ReplResolver::new(9100, addressing, names);
    let answer = timed(&resolver, &Peer::new("workers-1", "10.0.0.7")).await;
    assert_eq!(
        answer,
        Some((Some(SocketAddr::from(([10, 0, 0, 7], 9100))), Duration::from_secs(2))),
        "the pod IP serves once the name lookup is past its bound"
    );
}

#[tokio::test(start_paused = true)]
async fn an_ip_host_is_never_looked_up() {
    let names = Arc::new(SilentResolver::default());
    let resolver = ReplResolver::new(9100, ReplAddressing::Static, names.clone());
    let answer = timed(&resolver, &Peer::new("w1", "10.0.0.2")).await;
    assert_eq!(answer, Some((Some(SocketAddr::from(([10, 0, 0, 2], 9100))), Duration::ZERO)));
    assert_eq!(names.lookups.load(Ordering::SeqCst), 0);
}
