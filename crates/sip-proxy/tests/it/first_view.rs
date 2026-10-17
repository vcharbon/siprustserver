//! A starting proxy's first worker view (ADR-0050 §5, §11): the probe sends
//! its first OPTIONS round as soon as the registry holds its first listing,
//! the view is in once every probe of that round has answered or run out its
//! reply window, and before it a request with no target is dropped and
//! counted, never answered `503`, so the sender's retransmission meets the
//! view.

use crate::common;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scenario_harness::{Harness, ServerTxn};
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts, InDialogMethod};
use sip_message::parser::custom::CustomParser;
use sip_message::types::SipHeader;
use sip_message::worker_report::{incarnation_value, Readiness, X_INCARNATION, X_READINESS};
use sip_message::{serialize, SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_proxy::health::{FirstView, HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::observability::metrics::NoTargetDrop;
use sip_proxy::registry::composed::ComposedWorkerRegistry;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::time::Instant;
use topology::{Peer, SimulatedMembership};

const PROXY: &str = "127.0.0.1:5080";
const WORKER: &str = "127.0.0.1:5071";
const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

/// The production cadence: a 1 s tick, a 1.5 s reply window.
const PRODUCTION: HealthProbeConfig =
    HealthProbeConfig { interval_ms: 1_000, timeout_ms: 1_500, threshold: 2 };

/// A tick long enough that only the first round leaves within a test.
const FIRST_ROUND_ONLY: HealthProbeConfig =
    HealthProbeConfig { interval_ms: 10_000, timeout_ms: 1_500, threshold: 2 };

fn hmac() -> Arc<StaticHmacKeyProvider> {
    Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap())
}

/// The worker `b2b-1` at [`WORKER`], read as `health`.
fn pool(health: WorkerHealth) -> Arc<SimulatedWorkerRegistry> {
    Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry {
            health,
            ..WorkerEntry::alive("b2b-1", ProxyAddr::new("127.0.0.1", 5071))
        }],
        Clock::test_at(0),
    ))
}

/// A load-balancing proxy over `registry`, its first view `view`.
async fn spawn_proxy(
    h: &Harness,
    lane: &str,
    registry: Arc<dyn WorkerRegistry>,
    view: Arc<FirstView>,
) -> common::ProxySut {
    let metrics = Arc::new(ProxyMetrics::new());
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac(),
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        metrics.clone(),
        Clock::test_at(0),
    ));
    let (ep, sock) = h.bind_sut(lane, PROXY).await;
    let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry)
        .clock(Clock::test_at(0))
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(metrics.clone())
        .first_view(view)
        .build(ep);
    common::ProxySut::from_task(sock, metrics, tokio::spawn(core.run()))
}

/// The OPTIONS probe over `registry`, latching `view`.
async fn spawn_probe(
    h: &Harness,
    registry: Arc<dyn WorkerRegistry>,
    control: Arc<dyn sip_proxy::registry::control::WorkerRegistryControl>,
    view: Arc<FirstView>,
    config: HealthProbeConfig,
) -> tokio::task::JoinHandle<()> {
    let (probe_ep, _) = h.bind_sut("probe", "127.0.0.1:5099").await;
    let probe = HealthProbe::new(
        probe_ep,
        registry,
        control,
        Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default())),
        Clock::test_at(0),
        Arc::new(IdGen::seeded(1)),
        config,
    )
    .with_first_view(view);
    tokio::spawn(probe.run())
}

/// Answer the probe's OPTIONS as a serving worker accepting new calls.
async fn answer_options(options: &mut ServerTxn) {
    let mut ok = options.respond(200, "OK");
    for line in common::options_capabilities() {
        ok = ok.with_header(&line.name, &line.value);
    }
    ok.with_header(X_READINESS, &Readiness::accept().header_value())
        .with_header(X_INCARNATION, &incarnation_value(1))
        .send()
        .await;
}

/// The VIP holder's proxy restarts under an established call: until its first
/// probe round has answered, the call's BYE and a new INVITE get no answer at
/// all, and each is counted; their retransmissions reach the worker once it
/// is read alive. The first OPTIONS leaves at bind, not a tick later. The
/// call ends with its BYE answered, the new call with the worker's final.
#[tokio::test(start_paused = true)]
async fn a_restarted_proxy_answers_nothing_before_its_first_worker_view() {
    let h = Harness::with_transit_delay("first-view-restart", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let worker = h.agent("b2b-1", WORKER).await;

    // The established call, through the proxy before its restart.
    let before = spawn_proxy(&h, "proxy", pool(WorkerHealth::Alive), FirstView::given()).await;
    let mut call = alice.invite(&worker).with_sdp(OFFER).through(before.addr()).send().await;
    let mut uas = worker.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    worker.receive("ACK").await;
    drop(before);
    tokio::time::sleep(Duration::from_millis(10)).await;

    // The restart: the worker unread, the view pending.
    let registry = pool(WorkerHealth::Unknown);
    let view = FirstView::pending();
    let started = Instant::now();
    let proxy = spawn_proxy(&h, "proxy-restarted", registry.clone(), view.clone()).await;
    let probe =
        spawn_probe(&h, registry.clone(), registry.control(), view.clone(), FIRST_ROUND_ONLY).await;

    // The first OPTIONS leaves at bind; its answer is held.
    let mut options = worker.receive("OPTIONS").await;
    assert!(started.elapsed() < Duration::from_millis(10), "probed at {:?}", started.elapsed());

    // Before the view: the BYE and a new INVITE are dropped, unanswered.
    let (mut bye, bye_sent) =
        dialog.send_request(InDialogMethod::Bye).try_send_with_request().await.unwrap();
    let mut second = alice.invite(&worker).through(proxy.addr()).send().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(proxy.metrics.no_target_dropped_count(NoTargetDrop::InDialog), 1);
    assert_eq!(proxy.metrics.no_target_dropped_count(NoTargetDrop::Initial), 1);
    assert!(!view.is_in());

    // The worker answers: the view is in.
    answer_options(&mut options).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(view.is_in());

    // Timer E and Timer A: the retransmissions are routed.
    tokio::time::sleep_until(started + Duration::from_millis(500)).await;
    alice
        .try_send_datagram(&serialize(&SipMessage::Request(bye_sent)), proxy.addr())
        .await
        .unwrap();
    worker.receive("BYE").await.respond(200, "OK").send().await;
    bye.expect(200).await;
    second.retransmit().await;
    worker.receive("INVITE").await.respond(486, "Busy Here").send().await;
    second.expect(486).await;
    worker.receive("ACK").await;

    assert_eq!(proxy.metrics.reject_count("no_target_available"), 0, "never a 503");
    probe.abort();
    let _ = h.finish().await;
}

/// The worker a simulated responder plays: whether it answers, and when each
/// OPTIONS arrived.
struct Responder {
    answers: bool,
    seen: Mutex<Vec<Instant>>,
}

fn spawn_responder(ep: Box<dyn UdpEndpoint>, worker: Arc<Responder>) {
    tokio::spawn(async move {
        let parser = CustomParser::new();
        while let Some(pkt) = ep.recv().await {
            let Ok(SipMessage::Request(req)) = parser.parse(&pkt.raw) else { continue };
            worker.seen.lock().unwrap().push(Instant::now());
            if !worker.answers {
                continue;
            }
            let extra: Vec<SipHeader> = common::options_capabilities()
                .into_iter()
                .chain([
                    SipHeader {
                        name: X_READINESS.into(),
                        value: Readiness::accept().header_value().into(),
                    },
                    SipHeader { name: X_INCARNATION.into(), value: incarnation_value(1).into() },
                ])
                .collect();
            let opts = GenerateResponseOpts {
                to_tag: Some("uas".into()),
                extra_headers: extra,
                ..Default::default()
            };
            let resp = generate_response(&req, 200, "OK", &opts);
            let _ = ep.send_to(&serialize(&SipMessage::Response(resp)), pkt.src).await;
        }
    });
}

/// No listed worker answers: the view is in once the first round's reply
/// window has run out, the worker still `Unknown` (not alive, not yet dead).
#[tokio::test(start_paused = true)]
async fn a_first_round_nobody_answers_completes_at_its_reply_window() {
    let h = Harness::with_transit_delay("first-view-silent", 0);
    let (worker_ep, _) = h.bind_sut("b2b-1", WORKER).await;
    let worker = Arc::new(Responder { answers: false, seen: Mutex::new(Vec::new()) });
    spawn_responder(worker_ep, worker.clone());
    let registry = pool(WorkerHealth::Unknown);
    let view = FirstView::pending();
    let started = Instant::now();
    let probe =
        spawn_probe(&h, registry.clone(), registry.control(), view.clone(), PRODUCTION).await;

    tokio::time::sleep(Duration::from_millis(1_400)).await;
    assert!(!view.is_in(), "the round's reply window is still open");
    let first = worker.seen.lock().unwrap()[0];
    assert!(first - started < Duration::from_millis(10), "probed at {:?}", first - started);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(view.is_in(), "every probe of the round ran out its window");
    assert_eq!(registry.resolve("b2b-1").unwrap().health, WorkerHealth::Unknown);
    probe.abort();
    let _ = h.finish().await;
}

/// A discovered pool: no round leaves before the watch's first listing, and
/// the first one leaves as soon as it is in.
#[tokio::test(start_paused = true)]
async fn the_first_round_leaves_with_the_first_listing() {
    let h = Harness::with_transit_delay("first-view-listing", 0);
    let (worker_ep, _) = h.bind_sut("b2b-1", WORKER).await;
    let worker = Arc::new(Responder { answers: true, seen: Mutex::new(Vec::new()) });
    spawn_responder(worker_ep, worker.clone());
    let membership = SimulatedMembership::unsynced(Clock::test_at(0));
    let registry = Arc::new(ComposedWorkerRegistry::spawn(
        Arc::new(membership.clone()),
        5071,
        Clock::test_at(0),
    ));
    let view = FirstView::pending();
    let probe =
        spawn_probe(&h, registry.clone(), registry.control(), view.clone(), PRODUCTION).await;

    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(worker.seen.lock().unwrap().is_empty(), "no probe before the listing");
    assert!(!view.is_in());

    let listed = Instant::now();
    membership.sync(vec![Peer::new("b2b-1", "127.0.0.1")]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let first = worker.seen.lock().unwrap()[0];
    assert!(first - listed < Duration::from_millis(100), "probed {:?} after", first - listed);
    assert!(view.is_in());
    assert_eq!(registry.resolve("b2b-1").unwrap().health, WorkerHealth::Alive);
    probe.abort();
    let _ = h.finish().await;
}
