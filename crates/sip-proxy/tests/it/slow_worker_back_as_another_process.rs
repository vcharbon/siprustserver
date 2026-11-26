//! A worker read `Slow` that answers as another process (ADR-0069): the
//! process that served its dialogs died, so its death is a dead verdict, held
//! in the routing or not, whatever the new process answers.
//!
//! Timing axes: two workers behind one production probe; one probe per worker
//! leaves per 1 s interval (reply window 1.6 s, slow window 2 s, threshold 2),
//! each answered as its script says after its `i`-th OPTIONS arrives (5 ms
//! transit each way). Nothing else is in flight. Paused clock, the probe and
//! the worker tasks of one current-thread runtime.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::CustomParser;
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::{HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use sip_txn::IdGen;

use crate::probe_late_answer_reading::{respond, Answer, Script};
use crate::probe_slow_verdict::{run_for, LATE, PROMPT, TRANSIT};

const A: (&str, &str) = ("w-a", "192.0.2.51:5070");
const B: (&str, &str) = ("w-b", "192.0.2.52:5070");

/// The probe of `w-a` and `w-b`, each answering as its script says.
struct Rig {
    registry: Arc<SimulatedWorkerRegistry>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Rig {
    async fn new(a: Script, b: Script) -> Self {
        let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
        let mut tasks = Vec::new();
        let mut entries = Vec::new();
        for ((id, addr), script) in [(A, a), (B, b)] {
            let at: std::net::SocketAddr = addr.parse().unwrap();
            let ep = net.bind_udp(BindUdpOpts::new(at, 4096)).await.unwrap();
            tasks.push(respond(ep, script));
            entries.push(WorkerEntry {
                health: WorkerHealth::Unknown,
                ..WorkerEntry::alive(id, ProxyAddr::from(at))
            });
        }
        let clock = Clock::test_at(0);
        let registry = Arc::new(SimulatedWorkerRegistry::with_clock(entries, clock.clone()));
        let probe_at: std::net::SocketAddr = "192.0.2.1:5098".parse().unwrap();
        let ep = net.bind_udp(BindUdpOpts::new(probe_at, 4096)).await.unwrap();
        let probe = HealthProbe::new(
            ep,
            CustomParser::new(),
            registry.clone(),
            registry.control(),
            Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
            clock,
            Arc::new(IdGen::seeded(11)),
            HealthProbeConfig::default(),
        );
        tasks.push(tokio::spawn(probe.run()));
        Self { registry, tasks }
    }

    fn health(&self, id: &str) -> Option<WorkerHealth> {
        self.registry.resolve(id).map(|w| w.health)
    }

    fn stop(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

/// `w-a` answers in time as process 1, then late twice (read `Slow`, its late
/// stamp fresh); with `held`, its endpoint flips not ready while `w-b` reads
/// ready (held `Slow`, the flip's verdict withheld). Process 1 answers nothing
/// more, and process 2 answers the probe sent at 6 s in time with `status`,
/// before any miss reads `w-a` dead. Returns the dead verdicts told by 8 s.
async fn slow_then_back_as_another_process(held: bool, status: u16) -> Vec<String> {
    let a: Script = Arc::new(move |i| match i {
        0 | 1 => Answer::ok(PROMPT, Some(1)),
        2 | 3 => Answer::ok(LATE, Some(1)),
        4 | 5 => None,
        _ => Some(Answer { delay: PROMPT, status, incarnation: Some(2), allow: None }),
    });
    let b: Script = Arc::new(|_| Answer::ok(PROMPT, Some(7)));
    let rig = Rig::new(a, b).await;
    let mut verdicts = rig.registry.read_dead();
    run_for(Duration::from_millis(5_000)).await;
    assert_eq!(rig.health(A.0), Some(WorkerHealth::Slow), "two late answers read it slow");
    if held {
        rig.registry.set_ready(A.0, false);
        assert_eq!(rig.health(A.0), Some(WorkerHealth::Slow), "held: its late answer is fresh");
    }
    assert!(verdicts.try_recv().is_err(), "no verdict while process 1 answers late");
    run_for(Duration::from_millis(3_000)).await;
    let told: Vec<String> = std::iter::from_fn(|| verdicts.try_recv().ok()).collect();
    if held {
        assert_eq!(rig.health(A.0), None, "process 2 is not routed on an endpoint not ready");
    }
    rig.stop();
    told
}

#[tokio::test(start_paused = true)]
async fn a_held_worker_back_as_another_process_answering_200_is_a_dead_verdict() {
    assert_eq!(slow_then_back_as_another_process(true, 200).await, [A.0]);
}

#[tokio::test(start_paused = true)]
async fn a_held_worker_back_as_another_process_answering_503_is_a_dead_verdict() {
    assert_eq!(slow_then_back_as_another_process(true, 503).await, [A.0]);
}

/// Not held, its endpoint ready: the same death, the same verdict.
#[tokio::test(start_paused = true)]
async fn a_slow_worker_back_as_another_process_is_a_dead_verdict() {
    assert_eq!(slow_then_back_as_another_process(false, 200).await, [A.0]);
}
