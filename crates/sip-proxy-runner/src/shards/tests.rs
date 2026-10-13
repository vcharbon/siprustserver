//! Two shard cores over one [`SharedRouting`], on the simulated fabric.

use std::net::SocketAddr;
use std::time::Duration;

use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
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
        clock,
    }
}

fn invite() -> Vec<u8> {
    format!(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {CALLER};branch=z9hG4bK-shard\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-shard\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: shard@127.0.0.1\r\n\
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
