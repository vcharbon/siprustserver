//! The stops and the exit each one takes, over a wired core on simulated
//! fabrics whose SIP endpoint the test can close (the transaction owner's
//! input ends, so its loop ends).

use std::future::{pending, ready};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::initial_invite::build_initial_call;
use b2bua::limiter::release_queue::ReleaseFlushOutcome;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{FnPeerResolver, PullerConfig, ReplicatingCallStore, ServeConfig};
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaDeps, ReplicationSetup};
use call::Call;
use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{
    BindUdpOpts, SendError, SignalingNetwork, UdpEndpoint, UdpEndpointCounters, UdpPacket,
};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::IdGen;
use tokio::sync::watch;
use topology::{Peer, SimulatedMembership};

use super::*;

const NODE: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)), 5080);
const CALLER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 9)), 5060);
const CALLEE: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 3)), 5090);
const BOUNDS: DrainBounds = DrainBounds {
    grace: Duration::from_secs(5),
    floor: Duration::from_secs(1),
    release_flush: Duration::from_secs(3),
    cdr_flush: Duration::from_secs(3),
};

#[tokio::test]
async fn a_withdrawal_with_no_signal_is_a_stop() {
    let stop = next_stop(pending(), pending(), ready(()), pending()).await;
    assert_eq!(stop, Stop::Withdrawn);
}

#[tokio::test]
async fn a_loop_that_ended_wins_over_a_signal() {
    let stop = next_stop(pending(), ready(()), pending(), ready("router")).await;
    assert_eq!(stop, Stop::LoopEnded("router"));
}

#[tokio::test]
async fn sigterm_and_interrupt_are_stops() {
    assert_eq!(next_stop(pending(), ready(()), pending(), pending()).await, Stop::Terminate);
    assert_eq!(next_stop(ready(()), pending(), pending(), pending()).await, Stop::Interrupt);
}

/// An endpoint whose input ends when the test closes it.
struct Closable {
    inner: Box<dyn UdpEndpoint>,
    closed: watch::Receiver<bool>,
}

#[async_trait]
impl UdpEndpoint for Closable {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        let mut closed = self.closed.clone();
        tokio::select! {
            packet = self.inner.recv() => packet,
            _ = closed.wait_for(|c| *c) => None,
        }
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.inner.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
}

/// A wired worker `w0` seeing itself routable, the lever that closes its SIP
/// endpoint, the membership it reads, a caller on its fabric, and a callee
/// that never answers.
struct Worker {
    core: B2buaCore,
    close: watch::Sender<bool>,
    membership: Arc<SimulatedMembership>,
    caller: Box<dyn UdpEndpoint>,
    _callee: Box<dyn UdpEndpoint>,
}

async fn worker() -> Worker {
    worker_with_cdr(|_| Arc::new(InMemoryCdrWriter::new())).await
}

/// [`worker`] writing its CDRs to the sink `cdr` builds over the core's
/// metrics registry.
async fn worker_with_cdr(cdr: impl FnOnce(&B2buaMetrics) -> Arc<dyn CdrWriter>) -> Worker {
    let metrics = B2buaMetrics::new();
    let clock = Clock::test_at(0);
    let net = SimulatedSignalingNetwork::new(1);
    let (close, closed) = watch::channel(false);
    let inner = net.bind_udp(BindUdpOpts::new(NODE, 256)).await.unwrap();
    let caller = net.bind_udp(BindUdpOpts::new(CALLER, 64)).await.unwrap();
    let _callee = net.bind_udp(BindUdpOpts::new(CALLEE, 64)).await.unwrap();
    let membership = Arc::new(SimulatedMembership::with_clock(Vec::new(), clock.clone()));
    let repl = SimulatedReplicationNetwork::with_delay(1);
    let listener = repl.listen(SocketAddr::from(([127, 0, 0, 2], 9600))).await.unwrap();
    let setup = ReplicationSetup {
        network: Arc::new(repl),
        membership: membership.clone(),
        store: Arc::new(ReplicatingCallStore::new(1, clock.clone())),
        listener,
        addr_resolver: Arc::new(FnPeerResolver(|_: &Peer| SocketAddr::from(([127, 0, 0, 2], 1)))),
        incarnation_gen: 1,
        puller: PullerConfig::default(),
        serve: ServeConfig::default(),
    };
    let deps = B2buaDeps {
        config: B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: NODE.ip().to_string(),
            sip_local_port: NODE.port(),
            // The test process's own load never refuses the call.
            overload_panic_elu_threshold: 1.1,
            ..Default::default()
        },
        decision: Arc::new(ScriptedDecisionEngine::route_all_to(
            CALLEE.ip().to_string(),
            CALLEE.port(),
        )),
        limiter: Arc::new(NoopLimiter),
        cdr: cdr(&metrics),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        clock,
        id_gen: Arc::new(IdGen::seeded(0xB2B1)),
        refusals: None,
        deferred_ceilings: None,
        replication: Some(setup),
        metrics,
        adaptation_http: None,
        compose: Default::default(),
        capacity: None,
    };
    let core = B2buaCore::spawn(Box::new(Closable { inner, closed }), deps);
    membership.add(Peer::new("w0", "w0"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    Worker { core, close, membership, caller, _callee }
}

impl Worker {
    fn withdraw(&self) {
        self.membership.set_conditions("w0", false, true);
    }

    fn close_sip(&self) {
        self.close.send_replace(true);
    }

    /// A call still being set up: its callee never answers.
    async fn ringing_call(&self) {
        let invite = "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-ring\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: ring@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             Content-Length: 0\r\n\r\n";
        self.caller.send_to(invite.as_bytes(), NODE).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(self.core.active_calls(), 1, "the call is live");
    }

    fn flushes(&self, outcome: ReleaseFlushOutcome) -> u64 {
        self.core.metrics().limiter().release_flushes_total(outcome)
    }
}

#[tokio::test(start_paused = true)]
async fn a_withdrawal_drains_and_exits_zero() {
    let w = worker().await;
    w.withdraw();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        serve_until_stop(&w.core, "test", BOUNDS, pending(), pending()),
    )
    .await
    .expect("the withdrawal stops the worker");
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(w.core.metrics().drain_exits("quiescent"), 1, "the drain ran");
}

/// SIGTERM drains a cordoned worker as a ready one: the drain runs, its exit
/// is counted, the process exits zero.
#[tokio::test(start_paused = true)]
async fn sigterm_drains_a_cordoned_worker_and_exits_zero() {
    let w = worker().await;
    w.core.readiness().cordon().expect("a ready worker takes a cordon");
    assert_eq!(w.core.readiness_state(), b2bua::repl::ReadinessState::Cordoned);
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        serve_until_stop(&w.core, "test", BOUNDS, pending(), ready(())),
    )
    .await
    .expect("SIGTERM stops the worker");
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(w.core.metrics().drain_exits("quiescent"), 1, "the drain ran");
    assert_eq!(w.core.readiness_state(), b2bua::repl::ReadinessState::Draining);
}

#[tokio::test(start_paused = true)]
async fn a_loop_task_that_ends_flushes_the_releases_and_exits_non_zero() {
    let w = worker().await;
    w.close_sip();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        serve_until_stop(&w.core, "test", BOUNDS, pending(), pending()),
    )
    .await
    .expect("the ended loop stops the worker");
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 1, "the release flush ran");
}

#[tokio::test(start_paused = true)]
async fn a_loop_task_that_ends_mid_drain_cuts_it_short_and_exits_non_zero() {
    let w = worker().await;
    w.ringing_call().await;
    w.withdraw();
    let start = tokio::time::Instant::now();
    let serve = serve_until_stop(&w.core, "test", BOUNDS, pending(), pending());
    let close = async {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(w.core.readiness().is_draining(), "the drain is under way");
        w.close_sip();
        pending::<()>().await;
    };
    let code = tokio::select! {
        code = serve => code,
        _ = close => unreachable!(),
    };
    assert_eq!(code, ExitCode::FAILURE);
    assert!(start.elapsed() < BOUNDS.grace, "cut short, not waited out: {:?}", start.elapsed());
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 1, "the release flush ran");
    for reason in ["quiescent", "caught_up", "grace", "grace_peers_behind"] {
        assert_eq!(w.core.metrics().drain_exits(reason), 0, "no drain exit: {reason}");
    }
}

/// A sink that takes `per_record` to write each record.
struct SlowSink {
    per_record: Duration,
    written: InMemoryCdrWriter,
}

#[async_trait]
impl CdrWriter for SlowSink {
    async fn write(&self, call: &Call, terminated_at: i64) {
        tokio::time::sleep(self.per_record).await;
        self.written.write(call, terminated_at).await;
    }

    async fn read_all(&self) -> Vec<CdrRecord> {
        self.written.read_all().await
    }

    async fn close(&self, _deadline: tokio::time::Instant) {}
}

/// A worker whose CDRs go through the production buffer to a sink taking
/// `per_record` per record.
async fn worker_with_slow_sink(per_record: Duration) -> (Worker, Arc<SlowSink>) {
    let sink = Arc::new(SlowSink { per_record, written: InMemoryCdrWriter::new() });
    let inner = sink.clone();
    let w = worker_with_cdr(|metrics| {
        Arc::new(BufferedCdrWriter::spawn(inner, 1_024, metrics.clone()))
    })
    .await;
    (w, sink)
}

fn a_terminated_call(n: usize) -> Call {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-cdr{n}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alice{n}\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: cdr{n}@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let req = match CustomParser::new().parse(raw.as_bytes()).expect("parse") {
        SipMessage::Request(r) => r,
        _ => panic!("expected a request"),
    };
    let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(&req, CALLER, &cfg, &mut sip_ids::TurnIds::fixed(n as u64), 1_000)
}

/// The CDRs queued when a SIGTERM drain ends are written before the worker
/// exits, within the CDR flush bound.
#[tokio::test(start_paused = true)]
async fn sigterm_exits_once_the_queued_cdrs_are_written() {
    let (w, sink) = worker_with_slow_sink(Duration::from_millis(500)).await;
    for n in 0..4 {
        w.core.cdr().write(&a_terminated_call(n), 2_000).await;
    }
    let code = serve_until_stop(&w.core, "test", BOUNDS, pending(), ready(())).await;
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(sink.written.snapshot().len(), 4, "every queued CDR is written before the exit");
    assert_eq!(w.core.metrics().cdr_dropped_total(), 0);
}

/// A sink slower than the CDR flush bound: what the bound leaves unwritten is
/// counted dropped, so no record leaves the process uncounted.
#[tokio::test(start_paused = true)]
async fn the_cdrs_the_flush_bound_gives_up_are_counted_dropped() {
    let (w, sink) = worker_with_slow_sink(Duration::from_secs(1)).await;
    for n in 0..10 {
        w.core.cdr().write(&a_terminated_call(n), 2_000).await;
    }
    let start = tokio::time::Instant::now();
    let code = serve_until_stop(&w.core, "test", BOUNDS, pending(), ready(())).await;
    assert_eq!(code, ExitCode::SUCCESS);
    let written = sink.written.snapshot().len() as u64;
    let dropped = w.core.metrics().cdr_dropped_total();
    assert!(written < 10, "the bound cut the flush short: {written} written");
    assert_eq!(written + dropped, 10, "{written} written, {dropped} dropped");
    assert!(start.elapsed() <= BOUNDS.grace + BOUNDS.release_flush + BOUNDS.cdr_flush);
}

/// The exits that run no drain close the CDR sink too.
#[tokio::test(start_paused = true)]
async fn an_interrupt_and_an_ended_loop_exit_once_the_queued_cdrs_are_written() {
    let (w, sink) = worker_with_slow_sink(Duration::from_millis(500)).await;
    for n in 0..4 {
        w.core.cdr().write(&a_terminated_call(n), 2_000).await;
    }
    let code = serve_until_stop(&w.core, "test", BOUNDS, ready(()), pending()).await;
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(sink.written.snapshot().len(), 4, "interrupt");

    let (w, sink) = worker_with_slow_sink(Duration::from_millis(500)).await;
    for n in 0..4 {
        w.core.cdr().write(&a_terminated_call(n), 2_000).await;
    }
    w.close_sip();
    let code = serve_until_stop(&w.core, "test", BOUNDS, pending(), pending()).await;
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(sink.written.snapshot().len(), 4, "loop ended");
}
