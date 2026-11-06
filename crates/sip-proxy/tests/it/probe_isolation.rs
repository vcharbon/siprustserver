//! The proxy's isolation verdict: no worker answered any probe within the
//! isolation window. It is the proxy's own reach, read before the per-worker
//! dead verdicts: a proxy cut off every worker is isolated one window after
//! the last answer it heard, while each worker still reads `Alive`; one worker
//! still answering keeps it reachable whatever the others do.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::received::observed_source;
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig, WorkerReach};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;
use tokio::time::Instant;

const WORKERS: [(&str, &str); 2] = [("w-a", "192.0.2.51:5070"), ("w-b", "192.0.2.52:5070")];
const PROBE: &str = "192.0.2.1:5098";
const TRANSIT: Duration = Duration::from_millis(5);
const WINDOW: Duration = Duration::from_millis(2_000);

/// A worker answering the first `answered` OPTIONS it sees (Timer E copies
/// absorbed), each at once; the instant of its last answer.
fn respond(
    ep: Box<dyn UdpEndpoint>,
    answered: usize,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Option<Instant>>>) {
    let ep: Arc<dyn UdpEndpoint> = Arc::from(ep);
    let last = Arc::new(Mutex::new(None));
    let log = last.clone();
    let task = tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut branches = HashSet::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let Some(branch) = req.via().first().branch().map(str::to_owned) else { continue };
            if !branches.insert(branch) || branches.len() > answered {
                continue;
            }
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            let Ok(ok) = generate_response(&req, 200, "OK", &opts) else { continue };
            *log.lock().unwrap() = Some(Instant::now());
            let _ = ep.send_to(ok.image(), pkt.src).await;
        }
    });
    (task, last)
}

/// What a run saw: when the proxy first read itself isolated, the last answer
/// any worker sent, and each worker's health at the isolation instant.
struct Run {
    isolated_at: Option<Instant>,
    last_answer: Option<Instant>,
    health_at_isolation: Vec<Option<WorkerHealth>>,
}

/// Probe both workers, worker `i` answering its first `answered[i]` probes,
/// until the proxy reads itself isolated or `run_for` has passed.
async fn run(answered: [usize; 2], run_for: Duration) -> Run {
    let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
    let mut responders = Vec::new();
    let mut lasts = Vec::new();
    let mut entries = Vec::new();
    for ((id, addr), answered) in WORKERS.iter().zip(answered) {
        let ep = net.bind_udp(BindUdpOpts::new(addr.parse().unwrap(), 4096)).await.unwrap();
        let (task, last) = respond(ep, answered);
        responders.push(task);
        lasts.push(last);
        entries.push(WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive(
                *id,
                ProxyAddr::from(addr.parse::<std::net::SocketAddr>().unwrap()),
            )
        });
    }
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(entries, Clock::test_at(0)));
    let reach = WorkerReach::unheard();
    let probe = HealthProbe::new(
        probe_ep,
        registry.clone(),
        registry.control(),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(12)),
        HealthProbeConfig::default(),
    )
    .with_reach(reach.clone());
    let task = tokio::spawn(probe.run());

    // Past the first answers, so the start's own isolation is behind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let until = Instant::now() + run_for;
    let mut isolated_at = None;
    while Instant::now() < until {
        if reach.isolated(WINDOW) {
            isolated_at = Some(Instant::now());
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let health_at_isolation =
        WORKERS.iter().map(|(id, _)| registry.resolve(id).map(|w| w.health)).collect();
    task.abort();
    for r in responders {
        r.abort();
    }
    let last_answer = lasts.iter().filter_map(|l| *l.lock().unwrap()).max();
    Run { isolated_at, last_answer, health_at_isolation }
}

/// Both workers fall silent after their second answer: the proxy reads itself
/// isolated one window after the last answer reached it, while neither worker
/// is read `Dead` yet.
#[tokio::test(start_paused = true)]
async fn a_proxy_cut_off_every_worker_is_isolated_one_window_after_the_last_answer() {
    let r = run([2, 2], Duration::from_secs(10)).await;
    let last = r.last_answer.expect("the workers answered");
    let isolated = r.isolated_at.expect("the cut-off proxy reads itself isolated");
    assert!(isolated > last + TRANSIT + WINDOW, "isolated before the window ran out");
    assert!(
        isolated <= last + TRANSIT + WINDOW + Duration::from_millis(2),
        "isolated {:?} after the last answer, past the window",
        isolated - last
    );
    assert!(
        r.health_at_isolation.iter().all(|h| *h == Some(WorkerHealth::Alive)),
        "the isolation verdict comes before any dead verdict: {:?}",
        r.health_at_isolation
    );
}

/// One worker silent from its second probe on, the other answering every
/// probe: the proxy keeps its reach.
#[tokio::test(start_paused = true)]
async fn one_answering_worker_keeps_the_proxy_reachable() {
    let r = run([1, usize::MAX], Duration::from_secs(10)).await;
    assert_eq!(r.isolated_at, None, "a proxy with one answering worker read itself isolated");
}
