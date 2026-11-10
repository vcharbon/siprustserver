//! A poll stuck in a call's turn, end to end, in a child process the watchdog
//! ends: real threads, the real clock, the runtime the worker binaries run on
//! and the process's watchdog at a 200 ms threshold, wired as
//! `RunnerBase::spawn` wires it. The child stands up a primary and its backup
//! on simulated fabrics, places a call, and spins one turn of it on the CPU
//! inside a service rule; the parent reads the child's exit code and what it
//! printed.

use std::future::pending;
use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use b2bua::cdr::{CdrWriter, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{FnPeerResolver, PullerConfig, ReplicatingCallStore, ServeConfig};
use b2bua::rules::{
    Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
    ServiceSeed, SERVICE_LAYER,
};
use b2bua::store::{CallStore, InMemoryCallStore, PartitionRole};
use b2bua::{B2buaCore, B2buaDeps, CallEvent, ReplicationSetup};
use poll_watchdog::Watchdog;
use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::IdGen;
use topology::{Peer, SimulatedMembership};

use super::*;
use crate::shutdown::serve_until_stop;

/// Set in the child's environment, naming its [`Spin`]: the child test runs
/// only there.
const CHILD: &str = "POLL_WATCHDOG_CHILD";
/// Set in the child's environment: the hard exit bound, shortened for the
/// case whose process ends there.
const HARD_EXIT_MS: &str = "POLL_WATCHDOG_HARD_EXIT_MS";
/// The stuck threshold the child's watchdog runs at.
const THRESHOLD: Duration = Duration::from_millis(200);
/// The bounds of the child worker's exit: the discharge returns as soon as
/// the backup applied it, so a passing run never waits them out.
const CHILD_BOUNDS: DrainBounds = DrainBounds {
    grace: Duration::from_secs(1),
    floor: Duration::from_millis(100),
    release_flush: Duration::from_secs(2),
    cdr_flush: Duration::from_millis(100),
};
/// How long the parent waits for the child.
const CHILD_BUDGET: Duration = Duration::from_secs(15);
/// The line the child prints once the backup holds no copy of the call.
const BACKUP_GONE: &str = "backup copy gone";
/// The topic of the internal event whose turn spins.
const SPIN_TOPIC: &str = "spin";
/// The topic of the internal event queued behind it, whose turn rejects the
/// caller.
const REJECT_TOPIC: &str = "reject";

const W0_SIP: SocketAddr = addr([127, 0, 0, 2], 5080);
const W1_SIP: SocketAddr = addr([127, 0, 0, 4], 5080);
const W0_REPL: SocketAddr = addr([127, 0, 0, 2], 9600);
const W1_REPL: SocketAddr = addr([127, 0, 0, 4], 9600);
const CALLER_AT: SocketAddr = addr([10, 0, 0, 9], 5060);
const CALLEE_AT: SocketAddr = addr([127, 0, 0, 3], 5090);

const fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])), port)
}

/// How the spinning turn behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spin {
    /// It never leaves the CPU.
    ForEver,
    /// It leaves the CPU once it spent three thresholds, then rejects the
    /// caller and ends the call.
    ThenReject,
}

impl Spin {
    fn name(self) -> &'static str {
        match self {
            Spin::ForEver => "for-ever",
            Spin::ThenReject => "then-reject",
        }
    }

    fn from_env() -> Option<Spin> {
        match std::env::var(CHILD).ok()?.as_str() {
            "for-ever" => Some(Spin::ForEver),
            "then-reject" => Some(Spin::ThenReject),
            _ => None,
        }
    }
}

/// Run this binary's child test with `spin`, `workers` runtime workers and a
/// hard exit bound of `hard_exit` when stated, and return what it did.
fn run_child(spin: Spin, workers: usize, hard_exit: Option<Duration>) -> Output {
    let exe = std::env::current_exe().expect("the test binary");
    let mut command = Command::new(exe);
    command
        .args(["shutdown::stuck::child_tests::child", "--exact", "--ignored", "--nocapture"])
        .env(CHILD, spin.name())
        .env("TOKIO_WORKER_THREADS", workers.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(bound) = hard_exit {
        command.env(HARD_EXIT_MS, bound.as_millis().to_string());
    }
    let mut child = command.spawn().expect("the child starts");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("the child's status reads").is_some() {
            return child.wait_with_output().expect("the child's output reads");
        }
        if started.elapsed() > CHILD_BUDGET {
            let _ = child.kill();
            let out = child.wait_with_output().expect("the child's output reads");
            panic!("the child outlived {CHILD_BUDGET:?}: {}", text(&out));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text(out: &Output) -> String {
    format!(
        "--- stdout\n{}\n--- stderr\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The watchdog's line: the stuck poll's task, its thread, its CPU and its
/// call.
fn watchdog_line(all: &str) -> &str {
    all.lines().find(|l| l.contains("a poll is stuck on the CPU")).unwrap_or("")
}

/// A call's turn spinning on the CPU ends the process with the watchdog's
/// code, after the call's discharge reached the backup: its copy there is
/// deleted, so no takeover runs the turn's input again.
#[test]
fn a_call_turn_spinning_for_ever_ends_the_process_and_its_discharge_reaches_the_backup() {
    let out = run_child(Spin::ForEver, 4, None);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(i32::from(poll_watchdog::EXIT_CODE)), "{all}");
    let line = watchdog_line(&all);
    for field in ["task=", "thread=", "cpu_ms=", "call_ref=\"w0|spin@10.0.0.9|alicetag\""] {
        assert!(line.contains(field), "the watchdog's line names {field}: {all}");
    }
    assert!(all.contains("discharge=\"backup_applied\""), "the discharge's outcome: {all}");
    assert!(all.contains(BACKUP_GONE), "the backup's copy is deleted: {all}");
    assert!(all.contains("cdrs=1"), "one CDR, the discharge's: {all}");
    assert!(all.contains("caller_heard=0"), "nothing reaches the caller: {all}");
}

/// The stuck thread is the runtime's only worker: nothing else of the
/// process runs, and the watchdog thread ends it at its bound.
#[test]
fn a_call_turn_spinning_on_the_only_worker_ends_the_process_at_the_hard_bound() {
    let out = run_child(Spin::ForEver, 1, Some(Duration::from_millis(200)));
    let all = text(&out);
    assert_eq!(out.status.code(), Some(i32::from(poll_watchdog::EXIT_CODE)), "{all}");
    assert!(all.contains("did not exit within 200 ms"), "the hard exit's line: {all}");
}

/// A turn slow but finite: read stuck, its call discharged, then it leaves
/// the CPU with a rejection and a teardown, an event that rejects the caller
/// queued behind it. Neither lands: the call has one CDR, the discharge's,
/// and the caller hears nothing.
#[test]
fn a_slow_turn_that_resumes_after_the_discharge_lands_nothing() {
    let out = run_child(Spin::ThenReject, 4, Some(Duration::from_secs(12)));
    let all = text(&out);
    assert_eq!(out.status.code(), Some(i32::from(poll_watchdog::EXIT_CODE)), "{all}");
    assert!(all.contains("discharge=\"backup_applied\""), "the discharge's outcome: {all}");
    assert!(all.contains("the spinning turn returned"), "the turn left the CPU: {all}");
    assert!(all.contains("cdrs=1"), "one CDR, the discharge's: {all}");
    assert!(all.contains("caller_heard=0"), "the turn's 480 never leaves: {all}");
}

/// Whether the spinning rule has returned.
static RETURNED: AtomicBool = AtomicBool::new(false);

/// The child: a primary `w0` and its backup `w1`, a ringing call on `w0`,
/// then one turn of it spun on the CPU by a service rule. The process leaves
/// through `std::process::exit`, by the stuck exit or the hard bound.
#[test]
#[ignore = "the child process of the stuck-poll tests, run by them"]
fn child() {
    let Some(spin) = Spin::from_env() else { return };
    let log = observe::init_production("stuck-child").expect("the log writer starts");
    let watchdog = Watchdog::process();
    let runtime = crate::runtime::runtime().expect("the runtime builds");
    let code = runtime.block_on(async {
        let pair = Pair::start(spin).await;
        let call_ref = pair.ringing_call().await;
        watch_core(watchdog, &pair.w0, THRESHOLD, CHILD_BOUNDS).expect("the watchdog starts");
        if let Some(ms) = std::env::var(HARD_EXIT_MS).ok().and_then(|v| v.parse().ok()) {
            watchdog.exit_after(Duration::from_millis(ms));
        }
        tokio::spawn(report_backup_gone(pair.w1_store.clone(), call_ref.clone()));
        pair.post(&call_ref, SPIN_TOPIC);
        if spin == Spin::ThenReject {
            pair.post(&call_ref, REJECT_TOPIC);
        }
        let (bounds, exit) = (CHILD_BOUNDS, Duration::from_secs(30));
        let code =
            serve_until_stop(&pair.w0, "child", bounds, exit, watchdog, pending(), pending()).await;
        if spin == Spin::ThenReject {
            let returned = |_: &Pair| RETURNED.load(Ordering::SeqCst);
            pair.until_within(Duration::from_secs(10), "the spinning turn returned", returned)
                .await;
            println!("the spinning turn returned");
        }
        // What the turn would send and the backup's report ride a few more
        // polls.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let cdrs = pair.w0_cdrs.read_all().await.len();
        let mut caller_heard = 0;
        while pair.caller.try_recv().is_some() {
            caller_heard += 1;
        }
        println!("cdrs={cdrs} caller_heard={caller_heard}");
        code
    });
    drop(log);
    let _ = std::io::stdout().flush();
    let code = if code == ExitCode::from(poll_watchdog::EXIT_CODE) {
        i32::from(poll_watchdog::EXIT_CODE)
    } else {
        1
    };
    std::process::exit(code);
}

/// Print [`BACKUP_GONE`] once `store` no longer holds `call_ref`'s copy.
async fn report_backup_gone(store: Arc<ReplicatingCallStore>, call_ref: String) {
    loop {
        let held = store.get_call(PartitionRole::Backup, "w0", &call_ref).await;
        if matches!(held, Ok(None)) {
            println!("{BACKUP_GONE}: {call_ref}");
            let _ = std::io::stdout().flush();
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The service whose rule spins on [`SPIN_TOPIC`] as the child's [`Spin`]
/// says.
fn spin_service() -> ServiceDef {
    fn rule(id: &'static str, topic: &'static str, handle: Handle) -> RuleDefinition {
        RuleDefinition {
            id,
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::internal_event().topic(topic),
            handle,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }
    fn rules() -> Vec<RuleDefinition> {
        vec![rule("spin", SPIN_TOPIC, spin), rule("reject", REJECT_TOPIC, |_| Some(reject()))]
    }
    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }
    ServiceDef { id: "spin", init, rules }
}

type Handle = fn(&RuleContext) -> Option<RuleHandleResult>;

/// The spinning rule: for ever, or until its thread spent three thresholds
/// of CPU, then [`reject`].
fn spin(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    let tid = poll_watchdog::schedstat::own_tid().expect("this thread's id");
    let cpu = || poll_watchdog::schedstat::cpu(tid).expect("this thread's CPU time");
    let from = cpu();
    while Spin::from_env() == Some(Spin::ForEver) || cpu() < from + THRESHOLD * 3 {
        std::hint::spin_loop();
    }
    RETURNED.store(true, Ordering::SeqCst);
    Some(reject())
}

/// A 480 to the caller and the call's end.
fn reject() -> RuleHandleResult {
    RuleHandleResult::new(vec![
        RuleAction::RespondToALeg {
            status: 480,
            reason: "Temporarily Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        },
        RuleAction::BeginTermination {
            reason: Some("spin".into()),
            cause: call::TerminationCause::Timeout(call::TimeoutKind::Setup),
            by_leg: None,
        },
    ])
}

struct Pair {
    w0: B2buaCore,
    w0_cdrs: InMemoryCdrWriter,
    w1: B2buaCore,
    w1_store: Arc<ReplicatingCallStore>,
    caller: Box<dyn UdpEndpoint>,
    _callee: Box<dyn UdpEndpoint>,
}

impl Pair {
    async fn start(spin: Spin) -> Pair {
        let clock = Clock::test_at(0);
        let sip = SimulatedSignalingNetwork::new(1);
        let repl = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let membership = Arc::new(SimulatedMembership::with_clock(Vec::new(), clock.clone()));
        let caller = sip.bind_udp(BindUdpOpts::new(CALLER_AT, 64)).await.expect("caller binds");
        let callee = sip.bind_udp(BindUdpOpts::new(CALLEE_AT, 64)).await.expect("callee binds");
        let parts = Parts { sip: &sip, repl: &repl, membership: &membership, clock: &clock };
        let w0_cdrs = InMemoryCdrWriter::new();
        let (w0, _) = parts.node("w0", W0_SIP, W0_REPL, &w0_cdrs).await;
        let (w1, w1_store) = parts.node("w1", W1_SIP, W1_REPL, &InMemoryCdrWriter::new()).await;
        for ordinal in ["w0", "w1"] {
            membership.add(Peer::new(ordinal, ordinal).with_identity(identity(ordinal)));
        }
        let pair = Pair { w0, w0_cdrs, w1, w1_store, caller, _callee: callee };
        pair.until("both workers bootstrapped", |p| {
            [&p.w0, &p.w1].iter().all(|w| w.supervisor().is_some_and(|s| s.all_bootstrapped()))
        })
        .await;
        tracing::info!(spin = spin.name(), "the pair is up");
        pair
    }

    /// A call on `w0` still being set up, its copy on `w1`: the front proxy's
    /// cookie names the pair. What the caller heard so far is read off.
    async fn ringing_call(&self) -> String {
        let invite = "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-spin\r\n\
             Record-Route: <sip:10.0.0.1:5060;v=3;w_pri=w0;w_bak=w1;e=0;kid=k1;sig=abc;lr>\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: spin@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             Content-Length: 0\r\n\r\n";
        self.caller.send_to(invite.as_bytes(), W0_SIP).await.expect("the INVITE leaves");
        self.until("the call is live on w0", |p| p.w0.active_calls() == 1).await;
        let calls = (self.w0.live_calls_probe())().expect("the live calls read");
        let call_ref = calls.first().expect("the call").call_ref.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let held = self.w1_store.get_call(PartitionRole::Backup, "w0", &call_ref).await;
            if matches!(held, Ok(Some(_))) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the backup never held the call");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        while self.caller.try_recv().is_some() {}
        call_ref
    }

    /// Post an internal event of `topic` to `call_ref` on `w0`.
    fn post(&self, call_ref: &str, topic: &str) {
        self.w0.post_event(CallEvent::InternalEvent {
            call_ref: call_ref.to_string(),
            topic: topic.into(),
            outcome: String::new(),
            payload: serde_json::Value::Null,
            body: Vec::new(),
            incarnation: None,
        });
    }

    async fn until(&self, what: &str, done: impl Fn(&Pair) -> bool) {
        self.until_within(Duration::from_secs(3), what, done).await;
    }

    async fn until_within(&self, within: Duration, what: &str, done: impl Fn(&Pair) -> bool) {
        let deadline = tokio::time::Instant::now() + within;
        while !done(self) {
            assert!(tokio::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn identity(ordinal: &str) -> String {
    format!("{ordinal}-boot-1")
}

/// The fabrics and the membership the pair shares.
struct Parts<'a> {
    sip: &'a SimulatedSignalingNetwork,
    repl: &'a Arc<SimulatedReplicationNetwork>,
    membership: &'a Arc<SimulatedMembership>,
    clock: &'a Clock,
}

impl Parts<'_> {
    async fn node(
        &self,
        ordinal: &str,
        sip_at: SocketAddr,
        repl_at: SocketAddr,
        cdrs: &InMemoryCdrWriter,
    ) -> (B2buaCore, Arc<ReplicatingCallStore>) {
        let endpoint = self.sip.bind_udp(BindUdpOpts::new(sip_at, 256)).await.expect("binds");
        let listener = self.repl.listen(repl_at).await.expect("repl listens");
        let store = Arc::new(ReplicatingCallStore::new(1, self.clock.clone()));
        let setup = ReplicationSetup {
            network: self.repl.clone(),
            membership: self.membership.clone(),
            store: store.clone(),
            listener,
            addr_resolver: Arc::new(FnPeerResolver(|p: &Peer| match p.ordinal.as_str() {
                "w0" => W0_REPL,
                _ => W1_REPL,
            })),
            incarnation_gen: 1,
            puller: PullerConfig::fast_test(),
            serve: ServeConfig::default(),
            replica: b2bua::ReplicaConfig::default(),
            write_path: Default::default(),
            verdict: Default::default(),
        };
        let deps = B2buaDeps {
            identity: identity(ordinal),
            config: B2buaConfig {
                self_ordinal: ordinal.into(),
                sip_local_ip: sip_at.ip().to_string(),
                sip_local_port: sip_at.port(),
                // The test process's own load never refuses the call.
                overload_panic_elu_threshold: 1.1,
                ..Default::default()
            },
            decision: Arc::new(ScriptedDecisionEngine::route_all_to(
                CALLEE_AT.ip().to_string(),
                CALLEE_AT.port(),
            )),
            limiter: Arc::new(NoopLimiter),
            cdr: Arc::new(cdrs.clone()),
            store: Arc::new(InMemoryCallStore::new()),
            store_faults: Default::default(),
            wire_faults: Default::default(),
            turn_probe: Default::default(),
            clock: self.clock.clone(),
            id_gen: Arc::new(IdGen::seeded(0xB2B1)),
            refusals: None,
            deferred_ceilings: None,
            replication: Some(setup),
            metrics: B2buaMetrics::new(),
            adaptation_http: None,
            compose: Default::default(),
            capacity: None,
        };
        (B2buaCore::spawn_with_services(endpoint, deps, vec![spin_service()]), store)
    }
}
