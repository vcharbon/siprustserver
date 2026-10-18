//! Two shard cores over one [`SharedRouting`], on the simulated fabric.

use std::net::SocketAddr;
use std::time::Duration;

use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::self_gate::{AdmitDecision, ShedReason};

use super::*;

const CALLER: &str = "127.0.0.1:5060";

/// A gate that refuses every new call.
struct Closed;

impl ProxySelfGate for Closed {
    fn try_admit_external(&self) -> AdmitDecision {
        AdmitDecision::Reject { reason: ShedReason::Cps, retry_after_sec: 3 }
    }
}

fn shared() -> SharedRouting {
    let clock = Clock::test_at(0);
    let id_gen = Arc::new(IdGen::seeded(7));
    SharedRouting {
        advertised: ProxyAddr::new("127.0.0.1", 5080),
        strategy: Arc::new(sip_proxy::ForwardAllStrategy::new(ProxyAddr::new("127.0.0.1", 5070))),
        registry: Arc::new(StaticWorkerRegistry::from_entries(vec![])),
        refusals: Arc::new(Refusals::new(&id_gen, 5)),
        id_gen,
        metrics: Arc::new(ProxyMetrics::new()),
        cancel_lru: Arc::new(CancelBranchLru::with_clock(clock.clone())),
        self_gate: Arc::new(Closed),
        intake_age: IntakeAgeRecorder::default(),
        resolver: ResolverConfig::default(),
        traces: Arc::new(ProxyTraces::from_env(0)),
        pulse: Arc::new(ShardPulse::new(2)),
        first_view: FirstView::given(),
        clock,
    }
}

fn invite() -> Vec<u8> {
    invite_on("shard")
}

/// A new call, `call` naming its branch, tag and Call-ID.
fn invite_on(call: &str) -> Vec<u8> {
    format!(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-{call}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{call}\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: {call}@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

async fn bind(net: &SimulatedSignalingNetwork, addr: &str) -> Box<dyn UdpEndpoint> {
    let addr: SocketAddr = addr.parse().unwrap();
    net.bind_udp(BindUdpOpts::new(addr, 64)).await.unwrap()
}

async fn reply(caller: &dyn UdpEndpoint) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), caller.recv())
        .await
        .expect("a reply")
        .expect("queue open")
        .raw
        .to_vec()
}

/// The INVITE is refused on one shard and its copy reaches the other: the
/// shards share one refusal memo, so the copy draws the same bytes and the
/// refusal is counted once.
#[tokio::test(start_paused = true)]
async fn a_copy_reaching_another_shard_draws_the_same_refusal() {
    let net = SimulatedSignalingNetwork::new(1);
    let shared = shared();
    let shard0 = shared.shard(0).build(bind(&net, "127.0.0.1:5080").await);
    let shard1 = shared.shard(1).build(bind(&net, "127.0.0.1:5081").await);
    let tasks = [tokio::spawn(shard0.run()), tokio::spawn(shard1.run())];
    let caller = bind(&net, CALLER).await;

    caller.send_to(&invite(), "127.0.0.1:5080".parse().unwrap()).await.unwrap();
    let first = reply(&*caller).await;
    caller.send_to(&invite(), "127.0.0.1:5081".parse().unwrap()).await.unwrap();
    let copy = reply(&*caller).await;

    assert!(first.starts_with(b"SIP/2.0 503"), "{}", String::from_utf8_lossy(&first));
    assert_eq!(String::from_utf8_lossy(&first), String::from_utf8_lossy(&copy));
    assert_eq!(shared.metrics.reject_count("proxy_overload_cps"), 1, "counted once");
    for t in tasks {
        t.abort();
    }
}

/// Whether `caller` stays unanswered for a second.
async fn unanswered(caller: &dyn UdpEndpoint) -> bool {
    tokio::time::timeout(Duration::from_secs(1), caller.recv()).await.is_err()
}

/// The shards, the health probe and the probe routes read ONE first view:
/// before the probe's first round a new call with no worker is dropped on
/// every shard and `/vrrp` and `/readyz` fail; once the round over the empty
/// pool is done, every shard refuses it and `/vrrp` passes.
#[tokio::test(start_paused = true)]
async fn the_shards_the_probe_and_the_probe_routes_read_one_first_view() {
    use probe_http::ProbeState;
    use sip_proxy::health::HealthProbeConfig;
    use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
    use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
    use sip_proxy::self_gate::AlwaysAdmitGate;
    use sip_proxy::LoadBalancerStrategy;
    use std::sync::atomic::AtomicBool;

    let net = SimulatedSignalingNetwork::new(1);
    let pool = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let base = shared();
    let strategy = Arc::new(LoadBalancerStrategy::new(
        pool.clone(),
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap()),
        observer.clone(),
        base.metrics.clone(),
        base.clock.clone(),
    ));
    let shared = SharedRouting {
        strategy,
        registry: pool.clone(),
        self_gate: Arc::new(AlwaysAdmitGate),
        first_view: FirstView::pending(),
        ..base
    };
    let shard0 = shared.shard(0).build(bind(&net, "127.0.0.1:5080").await);
    let shard1 = shared.shard(1).build(bind(&net, "127.0.0.1:5081").await);
    let tasks = [tokio::spawn(shard0.run()), tokio::spawn(shard1.run())];
    let routes = shared.probes(Arc::new(AtomicBool::new(false)), 2_000);
    let caller = bind(&net, CALLER).await;
    let shards: [SocketAddr; 2] =
        ["127.0.0.1:5080".parse().unwrap(), "127.0.0.1:5081".parse().unwrap()];

    for (n, shard) in shards.iter().enumerate() {
        caller.send_to(&invite_on(&format!("before-{n}")), *shard).await.unwrap();
        assert!(unanswered(&*caller).await, "shard {n} answered before the first view");
    }
    assert!(!(routes.vrrp)());
    assert_eq!((routes.ready)(), ProbeState::NotReady);

    let probe = shared.health_probe(
        bind(&net, "127.0.0.1:5099").await,
        pool.control(),
        observer,
        HealthProbeConfig::default(),
    );
    let probe = tokio::spawn(probe.run());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!((routes.vrrp)(), "the probe latched the view the routes read");
    for (n, shard) in shards.iter().enumerate() {
        caller.send_to(&invite_on(&format!("after-{n}")), *shard).await.unwrap();
        let answer = reply(&*caller).await;
        assert!(
            answer.starts_with(b"SIP/2.0 503"),
            "shard {n}: {}",
            String::from_utf8_lossy(&answer)
        );
    }
    probe.abort();
    for t in tasks {
        t.abort();
    }
}
