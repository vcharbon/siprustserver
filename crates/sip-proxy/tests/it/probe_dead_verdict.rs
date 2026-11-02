//! When the health probe reads a worker that falls silent `Dead`: each probe is
//! reaped at the end of its own reply window, so the verdict comes
//! [`HealthProbeConfig::verdict_after_first_miss`] after the first probe the
//! worker leaves unanswered was sent, never at a later tick.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;
use tokio::time::Instant;

const WORKER_ID: &str = "w-silent";
const WORKER: &str = "192.0.2.41:5070";
const PROBE: &str = "192.0.2.1:5098";
const TRANSIT_MS: u64 = 5;

/// Forwards every write to the registry and records when `Dead` was written.
struct DeadAt {
    inner: Arc<dyn WorkerRegistryControl>,
    at: Arc<Mutex<Option<Instant>>>,
}

impl DeadAt {
    fn note(&self, health: WorkerHealth) {
        if health == WorkerHealth::Dead {
            self.at.lock().unwrap().get_or_insert_with(Instant::now);
        }
    }
}

impl WorkerRegistryControl for DeadAt {
    fn set_health(&self, worker_id: &str, health: WorkerHealth) {
        self.note(health);
        self.inner.set_health(worker_id, health);
    }
    fn set_view(
        &self,
        worker_id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) {
        if let Some(health) = health {
            self.note(health);
        }
        self.inner.set_view(worker_id, health, acceptance);
    }
}

/// Answers the first `answered` OPTIONS at once, then falls silent: every
/// later OPTIONS is swallowed. Returns when the first swallowed one arrived.
fn answer_then_fall_silent(
    ep: Box<dyn UdpEndpoint>,
    answered: usize,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Option<Instant>>>) {
    let silent_at = Arc::new(Mutex::new(None));
    let seen = Arc::new(AtomicUsize::new(0));
    let first_swallowed = silent_at.clone();
    let task = tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            if seen.fetch_add(1, Ordering::SeqCst) >= answered {
                first_swallowed.lock().unwrap().get_or_insert_with(Instant::now);
                continue;
            }
            let opts = GenerateResponseOpts {
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            if let Ok(ok) = generate_response(&req, 200, "OK", &opts) {
                let _ = ep.send_to(ok.image(), pkt.src).await;
            }
        }
    });
    (task, silent_at)
}

/// Run the probe at the default cadence against a worker that answers
/// `answered` probes then falls silent. Returns when the first unanswered
/// probe was sent and when the worker was read `Dead`.
async fn silent_after(answered: usize) -> (Instant, Instant) {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let worker = net.bind_udp(BindUdpOpts::new(WORKER.parse().unwrap(), 4096)).await.unwrap();
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let (responder, silent_at) = answer_then_fall_silent(worker, answered);
    let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health: WorkerHealth::Unknown,
            ..WorkerEntry::alive(
                WORKER_ID,
                ProxyAddr::from(WORKER.parse::<std::net::SocketAddr>().unwrap()),
            )
        }],
        Clock::test_at(0),
    ));
    let dead_at = Arc::new(Mutex::new(None));
    let control = Arc::new(DeadAt { inner: registry.control(), at: dead_at.clone() });
    let probe = HealthProbe::new(
        probe_ep,
        registry,
        control,
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(11)),
        HealthProbeConfig::default(),
    );
    let task = tokio::spawn(probe.run());

    let until = Instant::now() + Duration::from_secs(20);
    while dead_at.lock().unwrap().is_none() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    task.abort();
    responder.abort();
    let arrived = silent_at.lock().unwrap().expect("a probe went unanswered");
    let dead = dead_at.lock().unwrap().expect("the silent worker is read dead");
    (arrived - Duration::from_millis(TRANSIT_MS), dead)
}

/// A worker that falls silent right after a tick, leaving that tick's probe
/// unanswered, is read `Dead` interval + reply window after the tick.
#[tokio::test(start_paused = true)]
async fn a_worker_silent_right_after_a_tick_is_read_dead_one_interval_and_window_later() {
    let config = HealthProbeConfig::default();
    let (tick, dead) = silent_after(2).await;
    let interval_and_window = Duration::from_millis(config.interval_ms + config.timeout_ms);
    assert_eq!(config.verdict_after_first_miss(), interval_and_window);
    assert!(dead >= tick + interval_and_window, "read dead before its last window ended");
    assert!(
        dead <= tick + interval_and_window + Duration::from_millis(TRANSIT_MS),
        "read dead {:?} after the tick, past interval + window ({interval_and_window:?})",
        dead - tick
    );
}

/// A worker that answers a tick's probe and falls silent right after is read
/// `Dead` within one more interval: by `verdict_within` of its silence.
#[tokio::test(start_paused = true)]
async fn a_worker_silent_right_after_answering_is_read_dead_within_the_bound() {
    let config = HealthProbeConfig::default();
    let (first_miss, dead) = silent_after(3).await;
    let silent_since = first_miss - config.period();
    assert!(
        dead >= first_miss + config.verdict_after_first_miss(),
        "read dead before its last window ended"
    );
    assert!(
        dead <= silent_since + config.verdict_within() + Duration::from_millis(TRANSIT_MS),
        "read dead {:?} after it fell silent, past {:?}",
        dead - silent_since,
        config.verdict_within()
    );
}
