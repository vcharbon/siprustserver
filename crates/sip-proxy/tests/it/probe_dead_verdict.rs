//! When the health probe reads a worker `Dead`: each probe is reaped at the
//! end of its own reply window, so the verdict comes
//! [`HealthProbeConfig::verdict_after_first_miss`] after the first unanswered
//! probe was sent, never at a later tick, and at most
//! [`HealthProbeConfig::verdict_within`] after the worker falls silent. An
//! answer counts only inside its window, its last instant included.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::received::observed_source;
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;
use tokio::time::Instant;

const WORKER_ID: &str = "w-silent";
const WORKER: &str = "192.0.2.41:5070";
const PROBE: &str = "192.0.2.1:5098";
const TRANSIT: Duration = Duration::from_millis(5);

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

/// How the simulated worker answers: each OPTIONS `delay` after it arrives,
/// the first `answered` of them only. A Timer E copy of an OPTIONS already
/// seen is absorbed, as its server transaction would.
#[derive(Clone, Copy)]
struct Answers {
    delay: Duration,
    answered: usize,
}

/// What the worker saw and did.
#[derive(Default)]
struct Seen {
    /// When each OPTIONS (first copy) arrived, in order.
    arrivals: Vec<Instant>,
    /// When the worker sent its last answer.
    last_answer: Option<Instant>,
}

fn respond(
    ep: Box<dyn UdpEndpoint>,
    answers: Answers,
) -> (tokio::task::JoinHandle<()>, Arc<Mutex<Seen>>) {
    let ep: Arc<dyn UdpEndpoint> = Arc::from(ep);
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = seen.clone();
    let task = tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut branches = HashSet::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let Some(branch) = req.via().first().branch().map(str::to_owned) else { continue };
            if !branches.insert(branch) {
                continue;
            }
            let index = {
                let mut seen = log.lock().unwrap();
                seen.arrivals.push(Instant::now());
                seen.arrivals.len() - 1
            };
            if index >= answers.answered {
                continue;
            }
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            let Ok(ok) = generate_response(&req, 200, "OK", &opts) else { continue };
            let (ep, log, src) = (ep.clone(), log.clone(), pkt.src);
            tokio::spawn(async move {
                tokio::time::sleep(answers.delay).await;
                log.lock().unwrap().last_answer = Some(Instant::now());
                let _ = ep.send_to(ok.image(), src).await;
            });
        }
    });
    (task, seen)
}

/// What one run of the probe left.
struct Run {
    /// When each probe the worker saw was sent.
    sent: Vec<Instant>,
    last_answer: Option<Instant>,
    dead: Option<Instant>,
    health: Option<WorkerHealth>,
}

/// Run the probe with `config` against a worker answering as `answers`, until
/// it reads the worker `Dead` or `run_for` has passed.
async fn run(config: HealthProbeConfig, answers: Answers, run_for: Duration) -> Run {
    let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
    let worker = net.bind_udp(BindUdpOpts::new(WORKER.parse().unwrap(), 4096)).await.unwrap();
    let probe_ep = net.bind_udp(BindUdpOpts::new(PROBE.parse().unwrap(), 4096)).await.unwrap();
    let (responder, seen) = respond(worker, answers);
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
    let dead = Arc::new(Mutex::new(None));
    let control = Arc::new(DeadAt { inner: registry.control(), at: dead.clone() });
    let probe = HealthProbe::new(
        probe_ep,
        sip_message::CustomParser::new(),
        registry.clone(),
        control,
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(11)),
        config,
    );
    let task = tokio::spawn(probe.run());

    let until = Instant::now() + run_for;
    while dead.lock().unwrap().is_none() && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    task.abort();
    responder.abort();
    let seen = seen.lock().unwrap();
    let dead = *dead.lock().unwrap();
    Run {
        sent: seen.arrivals.iter().map(|at| *at - TRANSIT).collect(),
        last_answer: seen.last_answer,
        dead,
        health: registry.resolve(WORKER_ID).map(|w| w.health),
    }
}

const LONG: Duration = Duration::from_secs(20);

/// A worker that falls silent right after a tick, leaving that tick's probe
/// unanswered, is read `Dead` interval + reply window after the tick.
#[tokio::test(start_paused = true)]
async fn a_worker_silent_right_after_a_tick_is_read_dead_one_interval_and_window_later() {
    let config = HealthProbeConfig::default();
    let r = run(config, Answers { delay: Duration::ZERO, answered: 2 }, LONG).await;
    let tick = r.sent[2];
    let dead = r.dead.expect("the silent worker is read dead");
    let interval_and_window = Duration::from_millis(config.interval_ms + config.timeout_ms);
    assert!(dead > tick + interval_and_window, "read dead before its last window ended");
    assert!(dead <= tick + config.verdict_after_first_miss());
    assert!(
        dead <= tick + interval_and_window + TRANSIT,
        "read dead {:?} after the tick, past interval + window ({interval_and_window:?})",
        dead - tick
    );
}

/// A worker answers each probe a little over one interval late, so each
/// answer retires the probe sent just before it. Falling silent right after
/// such an answer, it leaves the retired probe uncounted, and its verdict
/// comes close to `verdict_within` after its silence, never past it.
#[tokio::test(start_paused = true)]
async fn a_late_answer_that_retires_the_next_probe_brings_the_verdict_to_its_bound() {
    let config = HealthProbeConfig::default();
    let delay = config.period();
    let r = run(config, Answers { delay, answered: 3 }, LONG).await;
    let silent = r.last_answer.expect("the worker answered");
    let dead = r.dead.expect("the silent worker is read dead");
    let first_counted_miss = r.sent[4];
    assert!(first_counted_miss > silent + TRANSIT, "the probe sent before the silence is retired");
    assert!(
        dead >= first_counted_miss + config.verdict_after_first_miss(),
        "read dead before its last window ended"
    );
    assert!(
        dead <= silent + config.verdict_within(),
        "read dead {:?} after the silence, past {:?}",
        dead - silent,
        config.verdict_within()
    );
    assert!(
        dead + TRANSIT * 3 > silent + config.verdict_within(),
        "read dead {:?} after the silence: the bound {:?} is not reached",
        dead - silent,
        config.verdict_within()
    );
}

/// An answer landing 10 ms after its window ended, before the next tick, is
/// dropped: every probe is a miss, and the worker is read `Dead`.
#[tokio::test(start_paused = true)]
async fn an_answer_after_its_window_is_dropped_and_counts_as_a_miss() {
    let config = HealthProbeConfig::default();
    let window = Duration::from_millis(config.timeout_ms);
    let delay = window + Duration::from_millis(10) - TRANSIT * 2;
    assert!(window + Duration::from_millis(10) < config.period() * 2, "before the next tick");
    let r = run(config, Answers { delay, answered: usize::MAX }, LONG).await;
    let dead = r.dead.expect("a worker answering past every window is read dead");
    assert!(dead >= r.sent[0] + config.verdict_after_first_miss());
    assert!(dead <= r.sent[0] + config.verdict_after_first_miss() + TRANSIT);
    assert_eq!(r.health, Some(WorkerHealth::Dead));
}

/// An answer landing at the last instant of its window counts, even at a
/// threshold of one miss.
#[tokio::test(start_paused = true)]
async fn an_answer_at_the_last_instant_of_its_window_is_no_miss() {
    let config = HealthProbeConfig { threshold: 1, ..HealthProbeConfig::default() };
    let delay = Duration::from_millis(config.timeout_ms) - TRANSIT * 2;
    let r = run(config, Answers { delay, answered: usize::MAX }, Duration::from_secs(10)).await;
    assert!(r.sent.len() >= 8, "the probe ran its rounds");
    assert_eq!(r.dead, None, "an answer at its window's end was counted a miss");
    assert_eq!(r.health, Some(WorkerHealth::Alive));
}
