use super::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sip_net::{SendError, UdpEndpointCounters, UdpPacket};

/// Endpoint double that records every send.
#[derive(Default)]
struct CapturingEndpoint {
    sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
}

#[async_trait]
impl UdpEndpoint for CapturingEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.sent.lock().unwrap().push((buf.to_vec(), dst));
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

/// Scriptable resolver: counts calls, optionally delays, then returns the
/// configured outcome.
struct FakeResolver {
    calls: AtomicU64,
    delay: Duration,
    answer: Option<SocketAddr>,
}

impl FakeResolver {
    fn answering(addr: SocketAddr) -> Self {
        Self { calls: AtomicU64::new(0), delay: Duration::ZERO, answer: Some(addr) }
    }
    fn failing() -> Self {
        Self { calls: AtomicU64::new(0), delay: Duration::ZERO, answer: None }
    }
    fn slow(addr: SocketAddr, delay: Duration) -> Self {
        Self { calls: AtomicU64::new(0), delay, answer: Some(addr) }
    }
}

#[async_trait]
impl HostResolver for FakeResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> Option<SocketAddr> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.answer
    }
}

fn dst() -> SocketAddr {
    "10.9.9.9:5060".parse().unwrap()
}

fn forwarder(
    resolver: Arc<dyn HostResolver>,
    cfg: ResolverConfig,
) -> (NamedForwarder, Arc<CapturingEndpoint>, Arc<ProxyMetrics>) {
    let ep = Arc::new(CapturingEndpoint::default());
    let metrics = Arc::new(ProxyMetrics::new());
    let fwd = NamedForwarder::new(ep.clone(), resolver, cfg, Clock::test_at(0), metrics.clone());
    (fwd, ep, metrics)
}

async fn settle() {
    // Let the spawned resolve task run (and auto-advance past any delay).
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn resolves_once_then_serves_from_cache() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("uas.example", 5060);

    fwd.send(b"pkt1", &target).await;
    settle().await;
    fwd.send(b"pkt2", &target).await;

    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "second send must be a cache hit");
    let sent = ep.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|(_, d)| *d == dst()));
    assert_eq!(metrics.named_send_count("resolved"), 1);
    assert_eq!(metrics.named_send_count("cached"), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_lookup_is_negatively_cached_then_retried_after_ttl() {
    let resolver = Arc::new(FakeResolver::failing());
    let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("gone.example", 5060);

    fwd.send(b"pkt1", &target).await;
    settle().await;
    // Within the negative TTL: short-circuited, no second lookup.
    fwd.send(b"pkt2", &target).await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.named_send_count("dropped_negative"), 1);

    // Past the negative TTL: a fresh lookup is allowed.
    tokio::time::advance(Duration::from_millis(5_001)).await;
    fwd.send(b"pkt3", &target).await;
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert!(ep.sent.lock().unwrap().is_empty(), "nothing resolvable, nothing sent");
}

#[tokio::test(start_paused = true)]
async fn positive_entry_expires_and_picks_up_new_address() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, _ep, _metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("uas.example", 5060);

    fwd.send(b"pkt1", &target).await;
    settle().await;
    tokio::time::advance(Duration::from_millis(60_001)).await;
    fwd.send(b"pkt2", &target).await;
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "expired positive entry must re-resolve");
}

#[tokio::test(start_paused = true)]
async fn concurrent_sends_to_same_name_are_single_flight() {
    let resolver = Arc::new(FakeResolver::slow(dst(), Duration::from_secs(2)));
    let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("uas.example", 5060);

    fwd.send(b"pkt1", &target).await;
    fwd.send(b"pkt2", &target).await; // races the in-flight resolve → dropped
    assert_eq!(metrics.named_send_count("dropped_in_flight"), 1);
    tokio::task::yield_now().await; // let the resolve task start (it parks at its delay)
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "single-flight: one lookup per name");

    tokio::time::advance(Duration::from_millis(2_001)).await;
    settle().await;
    assert_eq!(ep.sent.lock().unwrap().len(), 1, "the claiming packet is sent after resolve");
}

#[tokio::test(start_paused = true)]
async fn in_flight_cap_bounds_spawned_resolves() {
    let resolver = Arc::new(FakeResolver::slow(dst(), Duration::from_secs(5)));
    let cfg = ResolverConfig { max_in_flight: 2, ..ResolverConfig::default() };
    let (fwd, _ep, metrics) = forwarder(resolver.clone(), cfg);

    for i in 0..5 {
        fwd.send(b"pkt", &ProxyAddr::new(format!("n{i}.example"), 5060)).await;
    }
    assert_eq!(metrics.named_send_count("dropped_in_flight"), 3);
    tokio::task::yield_now().await; // let the claimed tasks start (they park at their delay)
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "cap must bound concurrent resolves");
}

// ── absolute-first resolution ───────────────────────────────────────────

/// Pure candidate ordering: dotted names try the absolutely-qualified form
/// first (ONE lookup instead of ndots search expansion), falling back to
/// the raw name; single-label and already-absolute names go straight to
/// one raw attempt.
#[test]
fn resolution_candidates_absolute_first_for_dotted_hosts() {
    assert_eq!(
        resolution_candidates("uas.sip-test.svc.cluster.local"),
        vec![
            "uas.sip-test.svc.cluster.local.".to_string(),
            "uas.sip-test.svc.cluster.local".to_string()
        ],
    );
    assert_eq!(
        resolution_candidates("mrf"),
        vec!["mrf".to_string()],
        "single-label skips straight to raw"
    );
    assert_eq!(
        resolution_candidates("uas.example."),
        vec!["uas.example.".to_string()],
        "already-absolute names are not double-dotted"
    );
}

type Lookup = std::pin::Pin<Box<dyn std::future::Future<Output = Option<SocketAddr>>>>;

/// A lookup-recording double for [`resolve_first`]: scripts which query
/// strings answer, and records every attempt in order.
fn scripted_lookup(
    answers: &'static [&'static str],
) -> (Arc<Mutex<Vec<String>>>, impl Fn(String, u16) -> Lookup) {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let recorded = attempts.clone();
    let lookup = move |candidate: String, _port: u16| {
        recorded.lock().unwrap().push(candidate.clone());
        let answer = answers.contains(&candidate.as_str()).then(dst);
        Box::pin(async move { answer })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Option<SocketAddr>>>>
    };
    (attempts, lookup)
}

#[tokio::test]
async fn resolve_first_tries_the_absolute_form_first() {
    let (attempts, lookup) = scripted_lookup(&["uas.example."]);
    assert_eq!(resolve_first("uas.example", 5060, lookup).await, Some(dst()));
    assert_eq!(
        *attempts.lock().unwrap(),
        vec!["uas.example.".to_string()],
        "absolute answered — one lookup, no raw attempt"
    );
}

#[tokio::test]
async fn resolve_first_falls_back_to_the_raw_name_when_absolute_fails() {
    // The absolute form yields nothing (a short name that NEEDS kube
    // search-domain expansion) — the raw name is attempted second and wins.
    let (attempts, lookup) = scripted_lookup(&["uas.example"]);
    assert_eq!(resolve_first("uas.example", 5060, lookup).await, Some(dst()));
    assert_eq!(
        *attempts.lock().unwrap(),
        vec!["uas.example.".to_string(), "uas.example".to_string()],
        "fallback order must be absolute → raw"
    );
}

#[tokio::test]
async fn resolve_first_single_label_goes_straight_to_raw() {
    let (attempts, lookup) = scripted_lookup(&["mrf"]);
    assert_eq!(resolve_first("mrf", 5060, lookup).await, Some(dst()));
    assert_eq!(*attempts.lock().unwrap(), vec!["mrf".to_string()]);
}

// ── proactive refresh + prewarm ─────────────────────────────────────────
//
// Default config: positive TTL 60 s, margin = min(60 s/4, 5 s) = 5 s, so
// the refresh one-shot fires at exactly 55 s after a positive store. Each
// `advance` below moves to exactly ONE pending deadline (test-clock rule 2).

#[tokio::test(start_paused = true)]
async fn used_entry_is_refreshed_before_ttl_and_stays_warm() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("uas.example", 5060);

    fwd.send(b"pkt1", &target).await; // miss → resolve, positive store at t=0
    settle().await;
    fwd.send(b"pkt2", &target).await; // cache hit — marks the entry USED
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

    // Exactly the refresh deadline (55 s): the used entry re-resolves
    // off-path and re-arms; nothing is sent by the refresh itself.
    tokio::time::advance(Duration::from_millis(55_000)).await;
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "used entry must refresh before its TTL");
    assert_eq!(metrics.resolver_refresh_count("refreshed"), 1);
    assert_eq!(ep.sent.lock().unwrap().len(), 2, "refresh must not send anything");

    // Past the ORIGINAL 60 s TTL: still a warm hit — no cold miss, no
    // dropped or delayed datagram.
    tokio::time::advance(Duration::from_millis(6_000)).await;
    fwd.send(b"pkt3", &target).await;
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        2,
        "send after the old TTL must be a cache hit"
    );
    assert_eq!(metrics.named_send_count("cached"), 2);
    assert_eq!(metrics.named_send_count("dropped_in_flight"), 0);
    assert_eq!(metrics.named_send_count("dropped_negative"), 0);
    assert_eq!(ep.sent.lock().unwrap().len(), 3, "the post-TTL datagram goes out inline");
}

#[tokio::test(start_paused = true)]
async fn unused_entry_refresh_idle_stops_and_expires_normally() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("oneshot.example", 5060);

    fwd.send(b"pkt1", &target).await; // stored at t=0, never hit again
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

    // Refresh deadline: the entry was never USED → idle stop, no resolve.
    tokio::time::advance(Duration::from_millis(55_000)).await;
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "an unused entry must not be refreshed");
    assert_eq!(metrics.resolver_refresh_count("idle_stopped"), 1);
    assert_eq!(metrics.resolver_refresh_count("refreshed"), 0);

    // The entry expires at its natural TTL; the next send is an ordinary
    // cold miss (bounded churn: exactly one refresh decision per store).
    tokio::time::advance(Duration::from_millis(5_001)).await;
    fwd.send(b"pkt2", &target).await;
    settle().await;
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        2,
        "the idle-stopped entry must expire normally"
    );
    assert_eq!(
        metrics.resolver_refresh_count("idle_stopped"),
        1,
        "no further refresh churn before the new store's cycle"
    );
}

#[tokio::test(start_paused = true)]
async fn prewarmed_name_first_send_is_a_cache_hit() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("mrf.sip-test.svc.cluster.local", 6001);

    fwd.prewarm(vec![target.clone()]);
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "prewarm resolves off the serving path");
    assert_eq!(metrics.resolver_refresh_count("prewarmed"), 1);

    // The FIRST send is warm: outcome=cached, datagram out inline, zero
    // resolves on the send path.
    fwd.send(b"pkt1", &target).await;
    assert_eq!(
        metrics.named_send_count("cached"),
        1,
        "first send after prewarm must be a cache hit"
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "the send path must trigger no resolve");
    assert_eq!(
        ep.sent.lock().unwrap().len(),
        1,
        "the datagram goes out immediately (no resolve wait)"
    );
    assert_eq!(ep.sent.lock().unwrap()[0].1, dst());
}

#[tokio::test(start_paused = true)]
async fn pinned_name_keeps_refreshing_across_ttl_windows_without_sends() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("mrf.sip-test.svc.cluster.local", 6001);

    fwd.prewarm(vec![target.clone()]);
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);

    // Three full refresh cycles with ZERO sends: a pinned name refreshes
    // every cycle regardless of use (never idle-stops).
    for round in 1..=3u64 {
        tokio::time::advance(Duration::from_millis(55_000)).await;
        settle().await;
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            1 + round,
            "pinned refresh round {round}"
        );
    }
    assert_eq!(metrics.resolver_refresh_count("refreshed"), 3);
    assert_eq!(
        metrics.resolver_refresh_count("idle_stopped"),
        0,
        "a pinned name must never idle-stop"
    );

    // ~2.75 original TTLs later, still warm on the first send.
    fwd.send(b"pkt", &target).await;
    assert_eq!(metrics.named_send_count("cached"), 1);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn failed_prewarm_retries_at_the_refresh_cadence_and_stores_nothing() {
    let resolver = Arc::new(FakeResolver::failing());
    let (fwd, _ep, metrics) = forwarder(resolver.clone(), ResolverConfig::default());
    let target = ProxyAddr::new("notyet.sip-test.svc.cluster.local", 6001);

    fwd.prewarm(vec![target.clone()]);
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(metrics.resolver_refresh_count("prewarm_failed"), 1);

    // Pinned → retried at the refresh cadence (boot must not stay cold
    // forever because CoreDNS was slow at prewarm time).
    tokio::time::advance(Duration::from_millis(55_000)).await;
    settle().await;
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2, "pinned prewarm failure must retry");
    assert_eq!(metrics.resolver_refresh_count("failed"), 1);

    // A failed prewarm stores NO negative entry: the send path behaves
    // exactly as without prewarm (miss → its own single-flight resolve).
    fwd.send(b"pkt", &target).await;
    assert_eq!(
        metrics.named_send_count("dropped_negative"),
        0,
        "prewarm failure must not poison the send path"
    );
}

#[tokio::test(start_paused = true)]
async fn cache_is_capped() {
    let resolver = Arc::new(FakeResolver::answering(dst()));
    let cfg = ResolverConfig { max_entries: 3, max_in_flight: 64, ..ResolverConfig::default() };
    let (fwd, _ep, metrics) = forwarder(resolver.clone(), cfg);

    for i in 0..10 {
        fwd.send(b"pkt", &ProxyAddr::new(format!("n{i}.example"), 5060)).await;
        settle().await;
    }
    assert!(
        metrics.resolver_cache_size() <= 3,
        "cache must stay at the cap, got {}",
        metrics.resolver_cache_size()
    );
}
