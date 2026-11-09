//! The stops and the exit each one takes, over a wired core on simulated
//! fabrics whose SIP endpoint the test can close (the transaction owner's
//! input ends, so its loop ends).

use std::future::{pending, ready};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::cdr::{BufferedCdrWriter, CdrRecord, CdrWriter, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::initial_invite::build_initial_call;
use b2bua::limiter::release_queue::ReleaseFlushOutcome;
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, LimiterHeld, LimiterReports, NoopLimiter,
    RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{FnPeerResolver, PullerConfig, ReplicatingCallStore, ServeConfig};
use b2bua::store::InMemoryCallStore;
use b2bua::turn_probe::{Hold, TurnMark, TurnProbe};
use b2bua::{B2buaDeps, ReplicationSetup, TurnProbes};
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
/// The stall a worker exits on, as `B2BUA_LOOP_EXIT_MS` defaults.
const LOOP_EXIT: Duration = crate::liveness_env::DEFAULT_LOOP_EXIT;
/// How far past the exit threshold the wait for the loops reads a stall.
const WITHIN_TWO_POLLS: Duration = Duration::from_secs(2);

/// A watchdog no runtime thread registers with: no poll ever reads stuck.
fn unwatched() -> &'static Watchdog {
    static UNWATCHED: std::sync::OnceLock<Watchdog> = std::sync::OnceLock::new();
    UNWATCHED.get_or_init(Watchdog::new)
}

fn stuck_on(call_ref: Option<String>) -> Stuck {
    Stuck { task: None, thread: 1, cpu: Duration::from_secs(11), call_ref }
}

#[tokio::test]
async fn a_stuck_poll_wins_over_every_other_stop() {
    let out = LoopOut::Ended("router");
    let stuck = stuck_on(None);
    let stop =
        next_stop(ready(stuck.clone()), ready(()), ready(()), ready(()), ready(()), ready(out))
            .await;
    assert_eq!(stop, Stop::Stuck(stuck));
}

#[tokio::test]
async fn a_withdrawal_with_no_signal_is_a_stop() {
    let stop = next_stop(pending(), pending(), pending(), pending(), ready(()), pending()).await;
    assert_eq!(stop, Stop::Withdrawn);
}

#[tokio::test]
async fn a_loop_that_ended_wins_over_a_signal() {
    let out = LoopOut::Ended("router");
    let stop = next_stop(pending(), pending(), pending(), ready(()), pending(), ready(out)).await;
    assert_eq!(stop, Stop::Loop(out));
}

#[tokio::test]
async fn a_loop_stalled_past_its_exit_wins_over_a_signal() {
    let out = LoopOut::Stalled("router");
    let stop = next_stop(pending(), pending(), pending(), ready(()), pending(), ready(out)).await;
    assert_eq!(stop, Stop::Loop(out));
}

#[tokio::test]
async fn a_supersession_wins_over_the_loops_its_fence_ends() {
    let out = LoopOut::Ended("router");
    let stop = next_stop(pending(), ready(()), ready(()), pending(), pending(), ready(out)).await;
    assert_eq!(stop, Stop::Superseded);
}

#[tokio::test]
async fn sigterm_and_interrupt_are_stops() {
    let terminate =
        next_stop(pending(), pending(), pending(), ready(()), pending(), pending()).await;
    assert_eq!(terminate, Stop::Terminate);
    let interrupt =
        next_stop(pending(), pending(), ready(()), pending(), pending(), pending()).await;
    assert_eq!(interrupt, Stop::Interrupt);
}

/// An endpoint whose input ends when the test closes it, and whose sends
/// never complete once the test parks them.
struct Closable {
    inner: Box<dyn UdpEndpoint>,
    closed: watch::Receiver<bool>,
    parked: Arc<AtomicBool>,
}

#[async_trait]
impl UdpEndpoint for Closable {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if self.parked.load(Ordering::Relaxed) {
            return pending().await;
        }
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

/// A wired worker `w0` seeing itself routable, the levers that close its SIP
/// endpoint and park its sends, the membership it reads, a caller on its
/// fabric, and a callee that never answers.
struct Worker {
    core: B2buaCore,
    close: watch::Sender<bool>,
    parked: Arc<AtomicBool>,
    membership: Arc<SimulatedMembership>,
    caller: Box<dyn UdpEndpoint>,
    _callee: Box<dyn UdpEndpoint>,
}

async fn worker() -> Worker {
    worker_with_cdr(|_| Arc::new(InMemoryCdrWriter::new())).await
}

/// A router loop that stops at its next turn once wedged: everything queued for
/// it waits, and its pulse reads it on that one piece of work.
#[derive(Default)]
struct RouterWedge {
    wedged: AtomicBool,
}

impl TurnProbe for RouterWedge {
    fn reached(&self, _mark: &TurnMark<'_>) -> Option<Hold> {
        None
    }

    fn router_turn(&self, _node: &str) -> Option<Hold> {
        self.wedged.load(Ordering::Relaxed).then(|| Box::pin(pending()) as Hold)
    }
}

/// A limiter whose releases are never answered.
struct SilentReleases;

#[async_trait]
impl CallLimiter for SilentReleases {
    async fn admit(
        &self,
        key: &str,
        change: u64,
        held: &LimiterHeld,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        NoopLimiter.admit(key, change, held, entries, release_on_refusal).await
    }
    async fn release(&self, _keys: &[String]) -> ReleaseAnswer {
        pending().await
    }
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        NoopLimiter.refresh(calls).await
    }
    fn admit_budget(&self) -> Duration {
        NoopLimiter.admit_budget()
    }
    fn report_to(&self, reports: LimiterReports) {
        NoopLimiter.report_to(reports)
    }
}

/// [`worker`] writing its CDRs to the sink `cdr` builds over the core's
/// metrics registry.
async fn worker_with_cdr(cdr: impl FnOnce(&B2buaMetrics) -> Arc<dyn CdrWriter>) -> Worker {
    worker_on(cdr, Default::default(), Arc::new(NoopLimiter)).await
}

/// A worker whose router loop wedges on `wedge`, and the wedge.
async fn wedgeable() -> (Worker, Arc<RouterWedge>) {
    wedgeable_on(Arc::new(NoopLimiter)).await
}

/// [`wedgeable`] over `limiter`.
async fn wedgeable_on(limiter: Arc<dyn CallLimiter>) -> (Worker, Arc<RouterWedge>) {
    let wedge = Arc::new(RouterWedge::default());
    let probe = TurnProbes::new(wedge.clone());
    let w = worker_on(|_| Arc::new(InMemoryCdrWriter::new()), probe, limiter).await;
    (w, wedge)
}

/// [`worker_with_cdr`] with the turn probe `turn_probe`, over `limiter`.
async fn worker_on(
    cdr: impl FnOnce(&B2buaMetrics) -> Arc<dyn CdrWriter>,
    turn_probe: TurnProbes,
    limiter: Arc<dyn CallLimiter>,
) -> Worker {
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
        replica: b2bua::ReplicaConfig::default(),
        write_path: Default::default(),
        verdict: Default::default(),
    };
    let deps = B2buaDeps {
        identity: OWN_IDENTITY.into(),
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
        limiter,
        cdr: cdr(&metrics),
        store: Arc::new(InMemoryCallStore::new()),
        store_faults: Default::default(),
        wire_faults: Default::default(),
        turn_probe,
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
    let parked = Arc::new(AtomicBool::new(false));
    let endpoint = Closable { inner, closed, parked: parked.clone() };
    let core = B2buaCore::spawn(Box::new(endpoint), deps);
    membership.add(Peer::new("w0", "w0").with_identity(OWN_IDENTITY));
    tokio::time::sleep(Duration::from_millis(100)).await;
    Worker { core, close, parked, membership, caller, _callee }
}

/// The worker identity of the test's worker.
const OWN_IDENTITY: &str = "w0-boot-1";

impl Worker {
    fn withdraw(&self) {
        self.membership.set_conditions("w0", false, true);
    }

    /// The orchestrator binds the worker's ordinal to a recreation of it.
    fn supersede(&self) {
        self.membership.add(Peer::new("w0", "w0").with_identity("w0-boot-2"));
    }

    fn close_sip(&self) {
        self.close.send_replace(true);
    }

    /// From now on a send never completes: the transaction owner's next send
    /// keeps it on that turn.
    fn park_sends(&self) {
        self.parked.store(true, Ordering::Relaxed);
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

    /// The router takes one more piece of work and stays on it.
    async fn stall_router(&self, wedge: &RouterWedge) {
        wedge.wedged.store(true, Ordering::Relaxed);
        let options = "OPTIONS sip:worker@127.0.0.2:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-wedge\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:probe@example.com>;tag=probetag\r\n\
             To: <sip:worker@example.com>\r\n\
             Call-ID: wedge@10.0.0.9\r\n\
             CSeq: 1 OPTIONS\r\n\
             Content-Length: 0\r\n\r\n";
        self.caller.send_to(options.as_bytes(), NODE).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A router that stays on one piece of work ends the process at the exit
/// threshold, not before: the exit a loop task's end takes, its release flush
/// included.
#[tokio::test(start_paused = true)]
async fn a_router_that_stays_stalled_exits_non_zero_at_its_threshold() {
    let (w, wedge) = wedgeable().await;
    w.stall_router(&wedge).await;
    let start = tokio::time::Instant::now();
    let code = tokio::time::timeout(
        LOOP_EXIT * 2,
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending()),
    )
    .await
    .expect("the stall stops the worker");
    assert_eq!(code, ExitCode::FAILURE);
    let waited = start.elapsed();
    assert!(waited > LOOP_EXIT - Duration::from_millis(10), "not before it: {waited:?}");
    assert!(waited <= LOOP_EXIT + WITHIN_TWO_POLLS, "within two polls of it: {waited:?}");
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 1, "the release flush ran");
    assert!(w.core.readiness().is_draining(), "it reads draining on its way out");
}

/// A transaction owner that stays on one send ends the process at the exit
/// threshold, as a stalled router does: what a call turn awaits of the owner
/// is bounded by the worker's exit.
#[tokio::test(start_paused = true)]
async fn an_owner_that_stays_stalled_exits_non_zero_at_its_threshold() {
    let w = worker().await;
    w.park_sends();
    let start = tokio::time::Instant::now();
    w.ringing_call().await;
    let code = tokio::time::timeout(
        LOOP_EXIT * 2,
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending()),
    )
    .await
    .expect("the owner's stall stops the worker");
    assert_eq!(code, ExitCode::FAILURE);
    let waited = start.elapsed();
    assert!(waited > LOOP_EXIT, "not before it: {waited:?}");
    assert!(waited <= LOOP_EXIT + WITHIN_TWO_POLLS, "within two polls of it: {waited:?}");
    assert_eq!(w.core.loops().stalled(LOOP_EXIT), ["txn_owner"], "the owner, not the router");
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 1, "the release flush ran");
}

/// A turn under the exit threshold, however many follow it, stops nothing.
#[tokio::test(start_paused = true)]
async fn a_turn_under_the_exit_threshold_stops_nothing() {
    let (w, wedge) = wedgeable().await;
    w.stall_router(&wedge).await;
    let under = LOOP_EXIT - Duration::from_secs(2);
    let serve =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending());
    assert!(tokio::time::timeout(under, serve).await.is_err(), "the worker serves on");
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 0);
}

/// A limiter that never answers cannot hold the exit: the release flush gives
/// its releases up at its bound.
#[tokio::test(start_paused = true)]
async fn a_stalled_workers_exit_is_held_by_no_limiter() {
    let (w, wedge) = wedgeable_on(Arc::new(SilentReleases)).await;
    w.core.limiter().release("a-key");
    w.stall_router(&wedge).await;
    let start = tokio::time::Instant::now();
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending())
            .await;
    assert_eq!(code, ExitCode::FAILURE);
    let waited = start.elapsed();
    let bound = LOOP_EXIT + WITHIN_TWO_POLLS + BOUNDS.release_flush + BOUNDS.cdr_flush;
    assert!(waited <= bound, "{waited:?} past {bound:?}");
    assert_eq!(w.flushes(ReleaseFlushOutcome::GivenUp), 1, "given up at the flush's bound");
}

/// A loop that stays stalled during a drain cuts it short: a drain with a long
/// grace gets no further with its router wedged. The exit is the loop's.
#[tokio::test(start_paused = true)]
async fn a_loop_stalled_during_a_drain_cuts_it_short_and_exits_non_zero() {
    let (w, wedge) = wedgeable().await;
    w.ringing_call().await;
    let long_grace = DrainBounds { grace: LOOP_EXIT * 4, ..BOUNDS };
    let start = tokio::time::Instant::now();
    let serve =
        serve_until_stop(&w.core, "test", long_grace, LOOP_EXIT, unwatched(), pending(), ready(()));
    let stall = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(w.core.readiness().is_draining(), "the drain is under way");
        w.stall_router(&wedge).await;
        pending::<()>().await;
    };
    let code = tokio::select! {
        code = serve => code,
        _ = stall => unreachable!(),
    };
    assert_eq!(code, ExitCode::FAILURE);
    let waited = start.elapsed();
    assert!(waited < long_grace.grace, "cut short, not waited out: {waited:?}");
    assert_eq!(w.flushes(ReleaseFlushOutcome::Empty), 1, "the release flush ran");
    for reason in ["quiescent", "caught_up", "grace", "grace_peers_behind"] {
        assert_eq!(w.core.metrics().drain_exits(reason), 0, "no drain exit: {reason}");
    }
}

/// A drain whose grace ends before the exit threshold is held by no wedge: the
/// router reads none of what the grace asks of it, and the drain returns by its
/// own bound, the worker exiting as a drain does.
#[tokio::test(start_paused = true)]
async fn a_drain_with_its_router_stalled_ends_at_its_own_bound() {
    let (w, wedge) = wedgeable().await;
    w.ringing_call().await;
    w.stall_router(&wedge).await;
    let start = tokio::time::Instant::now();
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), ready(()))
            .await;
    assert_eq!(code, ExitCode::SUCCESS);
    let waited = start.elapsed();
    let bound = BOUNDS.grace + BOUNDS.release_flush * 2 + BOUNDS.cdr_flush;
    assert!(waited <= bound, "{waited:?} past {bound:?}");
    assert_eq!(w.core.metrics().drain_exits("grace"), 1, "the grace's exit, its call residual");
}

#[tokio::test(start_paused = true)]
async fn a_withdrawal_drains_and_exits_zero() {
    let w = worker().await;
    w.withdraw();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending()),
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
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), ready(())),
    )
    .await
    .expect("SIGTERM stops the worker");
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(w.core.metrics().drain_exits("quiescent"), 1, "the drain ran");
    assert_eq!(w.core.readiness_state(), b2bua::repl::ReadinessState::Draining);
}

/// A superseded worker stops at once: no drain of its live call, no release
/// flush, an exit non-zero (ADR-0055 §6).
#[tokio::test(start_paused = true)]
async fn a_superseded_worker_exits_non_zero_without_a_drain() {
    let w = worker().await;
    w.ringing_call().await;
    w.supersede();
    let code = tokio::time::timeout(
        Duration::from_millis(100),
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending()),
    )
    .await
    .expect("the supersession stops the worker at once");
    assert_eq!(code, ExitCode::FAILURE);
    assert!(w.core.is_superseded());
    for reason in ["quiescent", "caught_up", "grace", "grace_peers_behind"] {
        assert_eq!(w.core.metrics().drain_exits(reason), 0, "no drain exit: {reason}");
    }
    for outcome in
        [ReleaseFlushOutcome::Empty, ReleaseFlushOutcome::Sent, ReleaseFlushOutcome::GivenUp]
    {
        assert_eq!(w.flushes(outcome), 0, "no release flush: {outcome:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_loop_task_that_ends_flushes_the_releases_and_exits_non_zero() {
    let w = worker().await;
    w.close_sip();
    let code = tokio::time::timeout(
        Duration::from_secs(10),
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending()),
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
    let serve =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending());
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
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), ready(()))
            .await;
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
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), ready(()))
            .await;
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
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), ready(()), pending())
            .await;
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(sink.written.snapshot().len(), 4, "interrupt");

    let (w, sink) = worker_with_slow_sink(Duration::from_millis(500)).await;
    for n in 0..4 {
        w.core.cdr().write(&a_terminated_call(n), 2_000).await;
    }
    w.close_sip();
    let code =
        serve_until_stop(&w.core, "test", BOUNDS, LOOP_EXIT, unwatched(), pending(), pending())
            .await;
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(sink.written.snapshot().len(), 4, "loop ended");
}

/// The exit of a poll stuck in a call's turn: Draining, the call discharged
/// with nothing on the wire and its CDR written, the releases flushed, and
/// the watchdog's code.
#[tokio::test(start_paused = true)]
async fn a_stuck_exit_discharges_the_call_it_holds_and_exits_with_the_watchdog_code() {
    let cdrs = InMemoryCdrWriter::new();
    let sink = cdrs.clone();
    let w = worker_with_cdr(move |_| Arc::new(sink)).await;
    w.ringing_call().await;
    let calls = (w.core.live_calls_probe())().expect("the live calls read");
    let call_ref = calls.first().expect("the ringing call").call_ref.clone();
    while w.caller.try_recv().is_some() {}
    let code = stuck_exit(&w.core, "test", BOUNDS, stuck_on(Some(call_ref.clone()))).await;
    assert_eq!(code, ExitCode::from(poll_watchdog::EXIT_CODE));
    assert!(w.core.readiness().is_draining(), "the worker steers new calls away");
    assert_eq!(w.core.active_calls(), 0, "the call is discharged");
    let written = cdrs.snapshot();
    assert_eq!(written.len(), 1, "one CDR for the discharged call");
    assert_eq!(written.first().map(|r| r.call_ref.as_str()), Some(call_ref.as_str()));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(w.caller.try_recv().is_none(), "the caller is left as she stood: nothing on the wire");
    assert_eq!(w.flushes(ReleaseFlushOutcome::GivenUp), 0, "the release flush ran to its end");
}

/// A stuck poll serving no call exits with the watchdog's code all the same.
#[tokio::test(start_paused = true)]
async fn a_stuck_exit_serving_no_call_exits_with_the_watchdog_code() {
    let w = worker().await;
    w.ringing_call().await;
    let code = stuck_exit(&w.core, "test", BOUNDS, stuck_on(None)).await;
    assert_eq!(code, ExitCode::from(poll_watchdog::EXIT_CODE));
    assert_eq!(w.core.active_calls(), 1, "no call named: none discharged");
}

#[test]
fn the_hard_exit_waits_out_every_bound_of_the_stuck_exit_and_one_second() {
    let bound = stuck_exit_bound(BOUNDS);
    assert_eq!(bound, BOUNDS.release_flush * 2 + BOUNDS.cdr_flush + Duration::from_secs(1));
}
