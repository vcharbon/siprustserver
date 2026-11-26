//! When the health probe reads a worker `Slow` (ADR-0069): its probe answers
//! arrive past their reply window and inside the slow window. A `Slow` worker
//! is no dead verdict; it reads `Alive` again after `recover_probes` answers in
//! time and `Dead` once it answers nothing within the slow window.
//!
//! Timing axes: each probe is answered `delay(i)` after its `i`-th OPTIONS
//! arrives (5 ms transit each way); one probe leaves per 1 s interval, its
//! reply window 1.6 s, its slow window 2 s, threshold 2. Nothing else is in
//! flight. Every verdict is read on the paused clock, the probe and the worker
//! tasks of one current-thread runtime.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::received::observed_source;
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig, WorkerCapabilities, WorkerReach};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;
use tokio::time::Instant;

pub(crate) const WORKER_ID: &str = "w-late";
pub(crate) const WORKER: &str = "192.0.2.51:5070";
pub(crate) const TRANSIT: Duration = Duration::from_millis(5);

/// A late answer: past the 1.6 s reply window, inside the 2 s slow window.
pub(crate) const LATE: Duration = Duration::from_millis(1_800);
/// An answer in time.
pub(crate) const PROMPT: Duration = Duration::from_millis(10);

/// When the `i`-th OPTIONS is answered after it arrives; `None` drops it.
pub(crate) type Delays = Arc<dyn Fn(usize) -> Option<Duration> + Send + Sync>;

/// Every health write and late stamp, in order, forwarded to the registry.
struct Writes {
    inner: Arc<dyn WorkerRegistryControl>,
    log: Arc<Mutex<Vec<(Instant, WorkerHealth)>>>,
    late: Arc<Mutex<Vec<Instant>>>,
}

impl WorkerRegistryControl for Writes {
    fn set_health(&self, worker_id: &str, health: WorkerHealth) {
        self.log.lock().unwrap().push((Instant::now(), health));
        self.inner.set_health(worker_id, health);
    }
    fn set_view(
        &self,
        worker_id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) {
        if let Some(health) = health {
            self.log.lock().unwrap().push((Instant::now(), health));
        }
        self.inner.set_view(worker_id, health, acceptance);
    }
    fn note_late_answer(&self, worker_id: &str, fresh_for: Duration) {
        self.late.lock().unwrap().push(Instant::now());
        self.inner.note_late_answer(worker_id, fresh_for);
    }
}

/// A worker answering each first copy of an OPTIONS as `delays` says.
fn respond(ep: Box<dyn UdpEndpoint>, delays: Delays) -> tokio::task::JoinHandle<()> {
    let ep: Arc<dyn UdpEndpoint> = Arc::from(ep);
    tokio::spawn(async move {
        let parser = CustomParser::new();
        let mut branches = HashSet::new();
        let mut index = 0;
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            let Some(branch) = req.via().first().branch().map(str::to_owned) else { continue };
            if !branches.insert(branch) {
                continue;
            }
            let delay = delays(index);
            index += 1;
            let Some(delay) = delay else { continue };
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probed".into()),
                extra_headers: crate::common::options_capabilities(),
                ..Default::default()
            };
            let Ok(ok) = generate_response(&req, 200, "OK", &opts) else { continue };
            let (ep, src) = (ep.clone(), pkt.src);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = ep.send_to(ok.image(), src).await;
            });
        }
    })
}

/// One proxy's probe of the worker, with what it wrote and counted.
pub(crate) struct Proxy {
    pub(crate) registry: Arc<SimulatedWorkerRegistry>,
    pub(crate) metrics: Arc<ProxyMetrics>,
    pub(crate) reach: Arc<WorkerReach>,
    pub(crate) capabilities: Arc<WorkerCapabilities>,
    writes: Arc<Mutex<Vec<(Instant, WorkerHealth)>>>,
    late: Arc<Mutex<Vec<Instant>>>,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    pub(crate) async fn spawn(
        net: &SimulatedSignalingNetwork,
        probe: &str,
        config: HealthProbeConfig,
    ) -> Self {
        let ep = net.bind_udp(BindUdpOpts::new(probe.parse().unwrap(), 4096)).await.unwrap();
        let clock = Clock::test_at(0);
        let registry = Arc::new(SimulatedWorkerRegistry::with_clock(
            vec![WorkerEntry {
                health: WorkerHealth::Unknown,
                ..WorkerEntry::alive(
                    WORKER_ID,
                    ProxyAddr::from(WORKER.parse::<std::net::SocketAddr>().unwrap()),
                )
            }],
            clock.clone(),
        ));
        let (writes, late) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let control =
            Arc::new(Writes { inner: registry.control(), log: writes.clone(), late: late.clone() });
        let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
        let metrics = Arc::new(ProxyMetrics::new());
        let reach = WorkerReach::unheard();
        let capabilities = WorkerCapabilities::unheard();
        let probe = HealthProbe::new(
            ep,
            CustomParser::new(),
            registry.clone(),
            control,
            observer,
            clock,
            Arc::new(IdGen::seeded(u64::from(seed_of(probe)))),
            config,
        )
        .with_metrics(metrics.clone())
        .with_reach(reach.clone())
        .with_capabilities(capabilities.clone());
        let task = tokio::spawn(probe.run());
        Self { registry, metrics, reach, capabilities, writes, late, task }
    }

    pub(crate) fn health(&self) -> Option<WorkerHealth> {
        self.registry.resolve(WORKER_ID).map(|w| w.health)
    }

    pub(crate) fn first(&self, health: WorkerHealth) -> Option<Instant> {
        self.writes.lock().unwrap().iter().find(|(_, h)| *h == health).map(|(at, _)| *at)
    }

    /// When the probe read the worker's last late answer.
    fn last_late(&self) -> Option<Instant> {
        self.late.lock().unwrap().last().copied()
    }
}

/// A seed of its own per probe address, so two probes mint distinct branches.
fn seed_of(probe: &str) -> u32 {
    probe.bytes().map(u32::from).sum()
}

pub(crate) async fn net_and_worker(
    delays: Delays,
) -> (SimulatedSignalingNetwork, tokio::task::JoinHandle<()>) {
    let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
    let worker = net.bind_udp(BindUdpOpts::new(WORKER.parse().unwrap(), 4096)).await.unwrap();
    let responder = respond(worker, delays);
    (net, responder)
}

/// Advance the paused clock by `span` in 1 ms steps, so every reply and reap
/// on the way is read where it falls.
pub(crate) async fn run_for(span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A worker answering every probe late reads `Slow` on two proxies probing it
/// apart, at their second miss, and never `Dead`: no dead verdict, so no
/// takeover. Each counts its late answers and one `Slow` spell.
#[tokio::test(start_paused = true)]
async fn a_worker_answering_late_reads_slow_on_both_proxies_and_never_dead() {
    let config = HealthProbeConfig::default();
    let (net, responder) = net_and_worker(Arc::new(|_| Some(LATE))).await;
    let start = Instant::now();
    let a = Proxy::spawn(&net, "192.0.2.1:5098", config).await;
    run_for(Duration::from_millis(300)).await;
    let b = Proxy::spawn(&net, "192.0.2.2:5098", config).await;
    run_for(Duration::from_secs(15)).await;
    for p in [&a, &b] {
        assert_eq!(p.health(), Some(WorkerHealth::Slow));
        assert_eq!(p.first(WorkerHealth::Dead), None, "a late worker was read dead");
        assert_eq!(p.first(WorkerHealth::Alive), None, "a late answer revived the worker");
        assert!(p.metrics.probe_late_answers(WORKER_ID) >= 10);
        assert_eq!(p.metrics.worker_slow(WORKER_ID), 1);
        p.task.abort();
    }
    let slow_at = a.first(WorkerHealth::Slow).expect("read slow");
    assert!(
        slow_at <= start + config.verdict_after_first_miss() + TRANSIT,
        "read slow {:?} after the first probe, past its second miss",
        slow_at - start
    );
    responder.abort();
}

/// An answer 10 ms past the slow window is no answer: the worker reads `Dead`
/// at its second miss, as a silent one does.
#[tokio::test(start_paused = true)]
async fn an_answer_past_the_slow_window_is_none_and_the_worker_reads_dead() {
    let config = HealthProbeConfig::default();
    let past = config.slow_window() + Duration::from_millis(10) - TRANSIT * 2;
    let (net, responder) = net_and_worker(Arc::new(move |_| Some(past))).await;
    let start = Instant::now();
    let p = Proxy::spawn(&net, "192.0.2.1:5098", config).await;
    run_for(Duration::from_secs(5)).await;
    let dead = p.first(WorkerHealth::Dead).expect("read dead");
    assert!(dead <= start + config.verdict_after_first_miss() + TRANSIT);
    assert_eq!(p.first(WorkerHealth::Slow), None);
    assert_eq!(p.metrics.probe_late_answers(WORKER_ID), 0);
    p.task.abort();
    responder.abort();
}

/// A `Slow` worker answering in time again reads `Alive` at its third answer
/// in time in a row, not before.
#[tokio::test(start_paused = true)]
async fn a_slow_worker_reads_alive_after_three_answers_in_time() {
    let config = HealthProbeConfig::default();
    let delays: Delays = Arc::new(|i| Some(if i < 5 { LATE } else { PROMPT }));
    let (net, responder) = net_and_worker(delays).await;
    let p = Proxy::spawn(&net, "192.0.2.1:5098", config).await;
    run_for(Duration::from_millis(5_500)).await;
    assert_eq!(p.health(), Some(WorkerHealth::Slow), "two answers in time keep it slow");
    run_for(Duration::from_millis(2_000)).await;
    assert_eq!(p.health(), Some(WorkerHealth::Alive));
    let slow = p.first(WorkerHealth::Slow).expect("read slow");
    let alive = p.first(WorkerHealth::Alive).expect("read alive");
    assert!(alive > slow);
    assert_eq!(p.first(WorkerHealth::Dead), None);
    p.task.abort();
    responder.abort();
}

/// A `Slow` worker that falls silent reads `Dead` once no answer, late or
/// not, came within the slow window: within `slow_window + interval` of the
/// instant the probe read its last late answer, so the slow window lengthens
/// no verdict on silence.
#[tokio::test(start_paused = true)]
async fn a_slow_worker_falling_silent_reads_dead_within_the_silence_bound() {
    let config = HealthProbeConfig::default();
    assert!(config.slow_window_fits());
    let delays: Delays = Arc::new(|i| (i < 4).then_some(LATE));
    let (net, responder) = net_and_worker(delays).await;
    let p = Proxy::spawn(&net, "192.0.2.1:5098", config).await;
    run_for(Duration::from_secs(12)).await;
    let slow = p.first(WorkerHealth::Slow).expect("read slow");
    let dead = p.first(WorkerHealth::Dead).expect("read dead");
    let last_answer = p.last_late().expect("late answers read");
    assert!(slow < last_answer && last_answer < dead);
    let bound = config.slow_window() + config.period();
    assert!(
        dead <= last_answer + bound,
        "read dead {:?} after its last answer, past {bound:?}",
        dead - last_answer
    );
    p.task.abort();
    responder.abort();
}

/// A worker read `Dead` that answers in time again reads `Alive` at its first
/// answer, not after `recover_probes` as a `Slow` one does.
#[tokio::test(start_paused = true)]
async fn a_dead_worker_answering_again_reads_alive_at_its_first_answer() {
    let config = HealthProbeConfig::default();
    let delays: Delays = Arc::new(|i| (!(1..5).contains(&i)).then_some(PROMPT));
    let (net, responder) = net_and_worker(delays).await;
    let p = Proxy::spawn(&net, "192.0.2.1:5098", config).await;
    run_for(Duration::from_millis(500)).await;
    assert_eq!(p.health(), Some(WorkerHealth::Alive));
    run_for(Duration::from_millis(4_600)).await;
    assert!(p.first(WorkerHealth::Dead).is_some(), "the silent spell reads dead");
    assert_eq!(
        p.health(),
        Some(WorkerHealth::Alive),
        "back at the answer to the probe sent at 5 s"
    );
    p.task.abort();
    responder.abort();
}
