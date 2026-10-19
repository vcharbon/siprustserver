//! A health probe configured with a zero interval rounds every millisecond:
//! its loop runs, and the worker it probes is marked alive by its answers.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

const WORKER: &str = "192.0.2.40:5070";
const PROBE: &str = "192.0.2.1:5099";

/// Answers every OPTIONS `ep` receives with a 200 stating the stack set.
fn answer_options(ep: Box<dyn UdpEndpoint>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let opts = GenerateResponseOpts {
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            if let Ok(ok) = generate_response(&req, 200, "OK", &opts) {
                let _ = ep.send_to(ok.image(), pkt.src).await;
            }
        }
    })
}

#[tokio::test(start_paused = true)]
async fn a_zero_probe_interval_rounds_every_millisecond() {
    let net = SimulatedSignalingNetwork::new(1);
    let worker = net.bind_udp(BindUdpOpts::new(WORKER.parse().unwrap(), 4096)).await.unwrap();
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let responder = answer_options(worker);
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive(
                "w-zero",
                ProxyAddr::from(WORKER.parse::<std::net::SocketAddr>().unwrap()),
            )
        }],
        Clock::test_at(0),
    ));
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        registry.control(),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(9)),
        HealthProbeConfig { interval_ms: 0, timeout_ms: 500, threshold: 3 },
    );
    let task = tokio::spawn(probe.run());

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!task.is_finished(), "the probe loop runs");
    assert_eq!(registry.resolve("w-zero").map(|w| w.health), Some(WorkerHealth::Alive));

    task.abort();
    tokio::time::sleep(Duration::from_secs(40)).await;
    responder.abort();
}
