//! The loop over a scripted proxy and a recording writer, on the paused
//! clock: what it writes, when, and what it reports.

use std::sync::Mutex;

use super::*;
use crate::switch::Timing;

const POLL: Duration = Duration::from_millis(100);

fn cfg() -> GuardConfig {
    GuardConfig {
        fitness: "127.0.0.1:9090".parse().unwrap(),
        peers: vec!["192.0.2.1".parse().unwrap()],
        poll: POLL,
        timing: Timing { dead: Duration::from_millis(1_000), hold: Duration::from_millis(3_000) },
        metrics: "127.0.0.1:0".parse().unwrap(),
        nft: "nft".to_string(),
    }
}

/// A proxy whose reading the test sets.
#[derive(Clone)]
struct Scripted(Arc<Mutex<Reading>>);

impl Scripted {
    fn new(reading: Reading) -> Self {
        Self(Arc::new(Mutex::new(reading)))
    }
    fn set(&self, reading: Reading) {
        *self.0.lock().unwrap() = reading;
    }
}

impl Source for Scripted {
    async fn read(&self) -> Reading {
        self.0.lock().unwrap().clone()
    }
}

/// The rulesets written, each as the [`Write`] it carries, with its instant;
/// every write fails while `failing` is set.
#[derive(Clone, Default)]
struct Recorder {
    writes: Arc<Mutex<Vec<(Instant, Write)>>>,
    failing: Arc<Mutex<bool>>,
}

impl Writer for Recorder {
    async fn write(&self, script: String, _bound: Duration) -> Result<(), String> {
        if *self.failing.lock().unwrap() {
            return Err("nft refused".to_string());
        }
        let write = if script.contains(" timeout ") { Write::Refresh } else { Write::Withdraw };
        self.writes.lock().unwrap().push((Instant::now(), write));
        Ok(())
    }
}

impl Recorder {
    fn since(&self, at: Instant) -> Vec<(Duration, Write)> {
        let writes = self.writes.lock().unwrap();
        writes.iter().filter(|(t, _)| *t >= at).map(|(t, w)| (*t - at, *w)).collect()
    }
}

struct Running {
    proxy: Scripted,
    writer: Recorder,
    metrics: Arc<GuardMetrics>,
    terminate: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

fn start(first: Reading) -> Running {
    let proxy = Scripted::new(first);
    let writer = Recorder::default();
    let metrics = Arc::new(GuardMetrics::starting());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = {
        let (proxy, writer, metrics) = (proxy.clone(), writer.clone(), metrics.clone());
        tokio::spawn(async move {
            let terminate = async move {
                let _ = rx.await;
            };
            run(&cfg(), &proxy, &writer, &metrics, &Pulse::new(), terminate).await
        })
    };
    Running { proxy, writer, metrics, terminate: Some(tx), task }
}

fn metric(m: &GuardMetrics, line: &str) -> bool {
    m.prometheus_text().lines().any(|l| l == line)
}

/// Fit at the start: refreshed at once and then about every 300 ms. Unfit:
/// one withdrawing write within a poll. Fit again: let out after the hold.
#[tokio::test(start_paused = true)]
async fn the_loop_refreshes_withdraws_at_once_and_restores_after_the_hold() {
    let r = start(Reading::Fit);
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    let origin = r.writer.writes.lock().unwrap()[0].0;
    let refreshes: Vec<_> =
        r.writer.since(origin).into_iter().map(|(t, _)| t.as_millis()).collect();
    assert_eq!(refreshes, vec![0, 300, 600, 900]);
    assert!(metric(&r.metrics, "route_guard_withdrawn 0"));

    let unfit_at = Instant::now();
    r.proxy.set(Reading::Unfit("isolated".to_string()));
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let after = r.writer.since(unfit_at);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].1, Write::Withdraw);
    assert!(after[0].0 <= POLL * 2, "withdrawn {:?} after the unfit answer", after[0].0);
    assert_eq!(readiness(&r.metrics), ProbeState::NotReady, "a withdrawn node's pod is not Ready");
    assert!(metric(&r.metrics, "route_guard_withdrawn 1"));
    assert!(metric(&r.metrics, "route_guard_withdrawals_total{cause=\"unfit\"} 1"));

    let fit_at = Instant::now();
    r.proxy.set(Reading::Fit);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let back = r.writer.since(fit_at);
    let first = back.first().expect("let out after the hold");
    assert_eq!(first.1, Write::Refresh);
    assert!(first.0 >= Duration::from_millis(3_000) && first.0 <= Duration::from_millis(3_100));
    assert!(metric(&r.metrics, "route_guard_restores_total 1"));
    assert!(metric(&r.metrics, "route_guard_withdrawn 0"));
    assert_eq!(readiness(&r.metrics), ProbeState::Ready);
    r.task.abort();
}

/// A proxy that stops answering: nothing more is written, and the guard
/// reads its node withdrawn once the last refresh ran out.
#[tokio::test(start_paused = true)]
async fn a_silent_proxy_lets_the_last_refresh_run_out() {
    let r = start(Reading::Fit);
    tokio::time::sleep(Duration::from_millis(650)).await;
    let last = r.writer.writes.lock().unwrap().last().unwrap().0;
    r.proxy.set(Reading::NoAnswer("connection refused".to_string()));
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    assert!(r.writer.since(last + Duration::from_millis(1)).is_empty());
    assert!(metric(&r.metrics, "route_guard_withdrawals_total{cause=\"expired\"} 1"));
    assert!(metric(&r.metrics, "route_guard_withdrawn 1"));
    r.task.abort();
}

/// Terminating: the guard keeps refreshing a fit proxy, then exits at the
/// first reading with no answer, without writing.
#[tokio::test(start_paused = true)]
async fn a_terminating_guard_exits_once_the_proxy_is_gone() {
    let mut r = start(Reading::Fit);
    tokio::time::sleep(Duration::from_millis(150)).await;
    r.terminate.take().unwrap().send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(!r.task.is_finished(), "a fit proxy keeps its guard");
    let writes = r.writer.writes.lock().unwrap().len();
    r.proxy.set(Reading::NoAnswer("connection refused".to_string()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(r.task.is_finished());
    assert_eq!(r.task.await.unwrap(), Ok(()));
    assert_eq!(r.writer.writes.lock().unwrap().len(), writes, "no write on the way out");
}

/// A guard whose first ruleset is refused does not run.
#[tokio::test(start_paused = true)]
async fn a_first_ruleset_refused_stops_the_guard() {
    let proxy = Scripted::new(Reading::Fit);
    let writer = Recorder::default();
    *writer.failing.lock().unwrap() = true;
    let e = run(&cfg(), &proxy, &writer, &GuardMetrics::default(), &Pulse::new(), async {})
        .await
        .unwrap_err();
    assert!(e.contains("nft refused"), "{e}");
}

/// A proxy not fit at the start: the first write withdraws the node.
#[tokio::test(start_paused = true)]
async fn a_guard_started_beside_a_proxy_not_fit_withdraws_first() {
    let r = start(Reading::NoAnswer("connection refused".to_string()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(r.writer.writes.lock().unwrap()[0].1, Write::Withdraw);
    assert!(metric(&r.metrics, "route_guard_withdrawals_total{cause=\"start\"} 1"));
    r.task.abort();
}

/// Before its first write the guard is not Ready; once a fit proxy's node is
/// let out it is, and a blip that withdraws the node makes it NotReady until
/// the hold has passed and the node is let out again, so a rollout counts
/// `minReadySeconds` only from a node that carries traffic.
#[tokio::test(start_paused = true)]
async fn the_guard_is_ready_exactly_while_its_node_is_let_out() {
    assert_eq!(readiness(&GuardMetrics::starting()), ProbeState::NotReady);
    let r = start(Reading::Fit);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(readiness(&r.metrics), ProbeState::Ready);
    r.proxy.set(Reading::Unfit("isolated".to_string()));
    tokio::time::sleep(Duration::from_millis(250)).await;
    r.proxy.set(Reading::Fit);
    for _ in 0..29 {
        tokio::time::sleep(POLL).await;
        assert_eq!(readiness(&r.metrics), ProbeState::NotReady, "inside the hold");
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(readiness(&r.metrics), ProbeState::Ready);
    r.task.abort();
}
