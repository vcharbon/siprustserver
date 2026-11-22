//! What a probe answer says of a worker, late or in time (ADR-0069): a late
//! answer stamps the worker only when it would read it `Alive` and comes from
//! the process last read; only a `200` in time writes the capabilities the
//! workers advertise; a worker never read serving on an endpoint that reads
//! not ready, another reading ready, is no dead verdict however it answered;
//! and late answers keep the proxy's reach, which their onset interrupts for at
//! most the rise in lateness past the isolation window.
//!
//! Timing axes: each probe is answered as its script says after its `i`-th
//! OPTIONS arrives (5 ms transit each way); one probe leaves per 1 s interval,
//! its reply window 1.6 s, its slow window 2 s, threshold 2. Nothing else is
//! in flight. Every verdict is read on the paused clock, the probe and the
//! worker tasks of one current-thread runtime.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::received::observed_source;
use sip_message::types::SipHeader;
use sip_message::worker_report::{incarnation_value, X_INCARNATION};
use sip_message::{CustomParser, SipMessage, SipParser};
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_proxy::health::HealthProbeConfig;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::ProxyAddr;
use tokio::time::Instant;

use crate::probe_slow_verdict::{run_for, Proxy, LATE, PROMPT, TRANSIT, WORKER, WORKER_ID};

const PROBE: &str = "192.0.2.1:5098";
const READY_WORKER_ID: &str = "w-ready";
const READY_WORKER: &str = "192.0.2.52:5070";
const ISOLATION: Duration = Duration::from_millis(2_000);

/// How the worker answers one OPTIONS: after `delay`, `status`, stating
/// `incarnation` when it is `Some`, and `allow` as its `Allow` when it is
/// `Some`, the default capabilities otherwise.
#[derive(Clone, Copy)]
pub(crate) struct Answer {
    pub(crate) delay: Duration,
    pub(crate) status: u16,
    pub(crate) incarnation: Option<u64>,
    pub(crate) allow: Option<&'static str>,
}

impl Answer {
    pub(crate) fn ok(delay: Duration, incarnation: Option<u64>) -> Option<Self> {
        Some(Self { delay, status: 200, incarnation, allow: None })
    }
}

/// The answer to the `i`-th OPTIONS; `None` leaves it unanswered.
pub(crate) type Script = Arc<dyn Fn(usize) -> Option<Answer> + Send + Sync>;

/// A worker answering each first copy of an OPTIONS as `script` says.
pub(crate) fn respond(ep: Box<dyn UdpEndpoint>, script: Script) -> tokio::task::JoinHandle<()> {
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
            let answer = script(index);
            index += 1;
            let Some(answer) = answer else { continue };
            let mut extra_headers = match answer.allow {
                Some(allow) => vec![SipHeader { name: "Allow".into(), value: allow.into() }],
                None => crate::common::options_capabilities(),
            };
            if let Some(gen) = answer.incarnation {
                extra_headers.push(SipHeader {
                    name: X_INCARNATION.into(),
                    value: incarnation_value(gen).into(),
                });
            }
            let opts = GenerateResponseOpts {
                incoming_source: Some(observed_source(pkt.src)),
                to_tag: Some("probed".into()),
                extra_headers,
                ..Default::default()
            };
            let reason = if answer.status == 200 { "OK" } else { "Service Unavailable" };
            let Ok(resp) = generate_response(&req, answer.status, reason, &opts) else { continue };
            let (ep, src) = (ep.clone(), pkt.src);
            tokio::spawn(async move {
                tokio::time::sleep(answer.delay).await;
                let _ = ep.send_to(resp.image(), src).await;
            });
        }
    })
}

async fn probed(script: Script) -> (Proxy, tokio::task::JoinHandle<()>) {
    let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
    let worker = net.bind_udp(BindUdpOpts::new(WORKER.parse().unwrap(), 4096)).await.unwrap();
    let responder = respond(worker, script);
    let p = Proxy::spawn(&net, PROBE, HealthProbeConfig::default()).await;
    (p, responder)
}

/// A worker answering `503` in time stays `Unknown`; answering `503` late it
/// is never read `Slow`: a `503` says nothing of whether it serves dialogs,
/// so its silence in time reads `Dead`.
#[tokio::test(start_paused = true)]
async fn a_late_503_never_reads_a_worker_slow() {
    let unavailable = |delay| Some(Answer { delay, status: 503, incarnation: None, allow: None });
    let (p, responder) = probed(Arc::new(move |_| unavailable(PROMPT))).await;
    run_for(Duration::from_secs(6)).await;
    assert_eq!(p.health(), Some(WorkerHealth::Unknown), "a 503 in time leaves it unknown");
    p.task.abort();
    responder.abort();

    let (p, responder) = probed(Arc::new(move |_| unavailable(LATE))).await;
    run_for(Duration::from_secs(6)).await;
    assert_eq!(p.first(WorkerHealth::Slow), None, "a late 503 read the worker slow");
    assert_eq!(p.health(), Some(WorkerHealth::Dead));
    assert_eq!(p.metrics.probe_late_answers(WORKER_ID), 0);
    p.task.abort();
    responder.abort();
}

/// The capabilities a supervision answer states are those of the last `200`
/// in time: a `503` in time, whatever `Allow` it carries, writes none.
#[tokio::test(start_paused = true)]
async fn only_a_200_writes_the_advertised_capabilities() {
    let script: Script = Arc::new(|i| {
        let (status, allow) = if i < 2 { (200, "INVITE, ACK, BYE") } else { (503, "OPTIONS") };
        Some(Answer { delay: PROMPT, status, incarnation: None, allow: Some(allow) })
    });
    let (p, responder) = probed(script).await;
    run_for(Duration::from_millis(4_500)).await;
    let lines: Vec<(String, String)> = p
        .capabilities
        .lines()
        .into_iter()
        .map(|h| (h.name.to_string(), h.value.to_string()))
        .collect();
    assert_eq!(lines, [("Allow".to_string(), "INVITE, ACK, BYE".to_string())]);
    p.task.abort();
    responder.abort();
}

/// A worker read `Alive` as one process whose late answers name another is a
/// process that died and one that boots: no late stamp, so its misses read
/// it `Dead`, and its calls are taken over.
#[tokio::test(start_paused = true)]
async fn a_late_answer_of_another_process_is_no_stamp() {
    let script: Script = Arc::new(|i| {
        let incarnation = Some(if i < 2 { 1 } else { 2 });
        Answer::ok(if i < 2 { PROMPT } else { LATE }, incarnation)
    });
    let (p, responder) = probed(script).await;
    run_for(Duration::from_secs(7)).await;
    assert!(p.first(WorkerHealth::Alive).is_some());
    assert_eq!(p.first(WorkerHealth::Slow), None, "another process's late answer read slow");
    assert_eq!(p.health(), Some(WorkerHealth::Dead));
    p.task.abort();
    responder.abort();
}

/// A restarted pod whose endpoint reads not ready while another member reads
/// ready, never read serving, that answers its probes late then falls silent:
/// out of the routing, it is never read `Slow`, and its `Dead` is no verdict,
/// so no dead notice names it. The ready member answers in time throughout.
#[tokio::test(start_paused = true)]
async fn a_booting_worker_answering_late_then_silent_draws_no_dead_notice() {
    let net = SimulatedSignalingNetwork::new(TRANSIT.as_millis() as u64);
    let bind = |addr: &str| net.bind_udp(BindUdpOpts::new(addr.parse().unwrap(), 4096));
    let booting = respond(
        bind(WORKER).await.unwrap(),
        Arc::new(|i| if i < 3 { Answer::ok(LATE, Some(9)) } else { None }),
    );
    let serving =
        respond(bind(READY_WORKER).await.unwrap(), Arc::new(|_| Answer::ok(PROMPT, None)));
    let p = Proxy::spawn(&net, PROBE, HealthProbeConfig::default()).await;
    let address = ProxyAddr::from(READY_WORKER.parse::<std::net::SocketAddr>().unwrap());
    p.registry.add(WorkerEntry::alive(READY_WORKER_ID, address));
    p.registry.set_ready(WORKER_ID, false);
    let mut verdicts = p.registry.read_dead();
    let judged = || p.registry.probed().into_iter().find(|w| w.id == WORKER_ID).map(|w| w.health);
    run_for(Duration::from_secs(3)).await;
    assert_eq!(judged(), Some(WorkerHealth::Dead), "a booting pod's misses read it dead");
    run_for(Duration::from_secs(6)).await;
    assert_eq!(judged(), Some(WorkerHealth::Dead));
    assert!(p.registry.resolve(READY_WORKER_ID).is_some_and(|w| w.is_alive()));
    assert!(verdicts.try_recv().is_err(), "a dead notice named a pod that never served");
    p.task.abort();
    booting.abort();
    serving.abort();
}

/// The only worker turns from answers in time to late ones. The proxy reads
/// itself isolated in one spell, from one isolation window after the last
/// answer in time to the first late one: at most one interval plus the rise
/// in lateness, less the window (ADR-0069 § Residuals). The late answers that
/// follow keep its reach for good.
#[tokio::test(start_paused = true)]
async fn the_onset_of_late_answers_reads_the_proxy_isolated_at_most_the_rise_past_the_window() {
    let script: Script = Arc::new(|i| Answer::ok(if i < 3 { PROMPT } else { LATE }, None));
    let (p, responder) = probed(script).await;
    run_for(Duration::from_millis(500)).await;
    assert!(!p.reach.isolated(ISOLATION), "the first answer gives reach");
    let (mut isolated, mut spells, mut was) = (Duration::ZERO, 0, false);
    let step = Duration::from_millis(1);
    let until = Instant::now() + Duration::from_secs(12);
    while Instant::now() < until {
        tokio::time::sleep(step).await;
        let now = p.reach.isolated(ISOLATION);
        spells += usize::from(now && !was);
        isolated += if now { step } else { Duration::ZERO };
        was = now;
    }
    assert_eq!(p.health(), Some(WorkerHealth::Slow));
    let config = HealthProbeConfig::default();
    let bound = config.period() + (LATE - PROMPT) - ISOLATION + TRANSIT * 2;
    assert_eq!(spells, 1, "late answers keep the reach once they arrive");
    assert!(isolated > Duration::ZERO, "a rise of {:?} reads isolated", LATE - PROMPT);
    assert!(isolated <= bound, "isolated {isolated:?}, past the {bound:?} the rise allows");
    p.task.abort();
    responder.abort();
}
