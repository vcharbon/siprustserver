//! The probe judges a member whose endpoint reads not ready as any other: from
//! the first listing, with no memory of an earlier view. A member that answers
//! reads `Alive`, and a view where none reads ready routes on it; members that
//! stay silent, as booting pods do, read `Dead` with no verdict and no worker to
//! route to, and the first view still latches.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::received::observed_source;
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{FirstView, HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

const ANSWERING: (&str, &str) = ("w-answers", "192.0.2.41:5070");
const SILENT: (&str, &str) = ("w-silent", "192.0.2.42:5070");
const PROBE: &str = "192.0.2.1:5098";
const TRANSIT_MS: u64 = 5;

/// Answer every OPTIONS `ep` receives 200.
fn answer(ep: Box<dyn UdpEndpoint>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut branches = HashSet::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let Some(branch) = req.via().first().branch().map(str::to_owned) else { continue };
            if !branches.insert(branch) {
                continue;
            }
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            let Ok(ok) = generate_response(&req, 200, "OK", &opts) else { continue };
            let _ = ep.send_to(ok.image(), pkt.src).await;
        }
    })
}

fn unprobed((id, at): (&str, &str)) -> WorkerEntry {
    WorkerEntry {
        health: WorkerHealth::Unknown,
        ..WorkerEntry::alive(id, ProxyAddr::from(at.parse::<std::net::SocketAddr>().unwrap()))
    }
}

/// Both members not ready from the first listing, the answering one answering
/// if `answers`; the probe has run `for_long`.
async fn probed(answers: bool, for_long: Duration) -> (SimulatedWorkerRegistry, Arc<FirstView>) {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let at = ANSWERING.1.parse().unwrap();
    let worker = net.bind_udp(BindUdpOpts::new(at, 4096)).await.unwrap();
    let responder = answers.then(|| answer(worker));
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![unprobed(ANSWERING), unprobed(SILENT)],
        Clock::test_at(0),
    );
    registry.set_ready(ANSWERING.0, false);
    registry.set_ready(SILENT.0, false);
    let view = FirstView::pending();
    let probe = HealthProbe::new(
        probe_ep,
        Arc::new(registry.clone()),
        registry.control(),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(11)),
        HealthProbeConfig::default(),
    )
    .with_first_view(view.clone());
    let task = tokio::spawn(probe.run());
    tokio::time::sleep(for_long).await;
    task.abort();
    if let Some(responder) = responder {
        responder.abort();
    }
    (registry, view)
}

/// The first round reads the member that answers alive: a proxy started while no
/// endpoint reads ready has a worker to route to, and its first view.
#[tokio::test(start_paused = true)]
async fn a_member_that_answers_while_none_reads_ready_is_routed_on_the_first_round() {
    let round = Duration::from_millis(HealthProbeConfig::default().timeout_ms + 1);
    let (registry, view) = probed(true, round).await;
    assert_eq!(registry.resolve(ANSWERING.0).map(|w| w.health), Some(WorkerHealth::Alive));
    assert!(view.is_in(), "the first round's window ended");
    assert!(registry.routes_on_answers());
}

/// A fresh rollout: every worker booting, none answering. No worker is alive,
/// no verdict is told, and the first view latches, so the proxy reads not ready
/// for what it is.
#[tokio::test(start_paused = true)]
async fn members_that_stay_silent_while_none_reads_ready_are_no_worker() {
    let (registry, view) = probed(false, HealthProbeConfig::default().verdict_within() * 2).await;
    assert!(view.is_in(), "the first round's window ended");
    assert!(registry.snapshot().iter().all(|w| !w.is_alive()), "no worker to route to");
    assert_eq!(registry.resolve(SILENT.0).map(|w| w.health), Some(WorkerHealth::Dead));
    assert_eq!(registry.read_dead_now(), [ANSWERING.0, SILENT.0], "read dead, on the probe alone");
}

/// The verdicts of a silent booting fleet: none, as they answered nothing.
#[tokio::test(start_paused = true)]
async fn a_booting_members_silence_is_no_verdict() {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let registry = SimulatedWorkerRegistry::with_clock(
        vec![unprobed(ANSWERING), unprobed(SILENT)],
        Clock::test_at(0),
    );
    registry.set_ready(ANSWERING.0, false);
    registry.set_ready(SILENT.0, false);
    let mut verdicts = registry.read_dead();
    let probe = HealthProbe::new(
        probe_ep,
        Arc::new(registry.clone()),
        registry.control(),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(11)),
        HealthProbeConfig::default(),
    );
    let task = tokio::spawn(probe.run());
    tokio::time::sleep(HealthProbeConfig::default().verdict_within() * 2).await;
    task.abort();
    assert!(verdicts.try_recv().is_err(), "no verdict told");
    assert_eq!(registry.resolve(SILENT.0).map(|w| w.health), Some(WorkerHealth::Dead));
}
