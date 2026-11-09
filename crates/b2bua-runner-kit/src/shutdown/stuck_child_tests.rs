//! A poll stuck in a call's turn, end to end, in a child process the watchdog
//! ends: real threads, the real clock, the runtime the worker binaries run on
//! and the process's watchdog at a 200 ms threshold. The child stands up a
//! primary and its backup on simulated fabrics, places a call, and spins one
//! turn of it on the CPU; the parent reads the child's exit code and what it
//! printed.

use std::future::pending;
use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use b2bua::cdr::InMemoryCdrWriter;
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::repl::{FnPeerResolver, PullerConfig, ReplicatingCallStore, ServeConfig};
use b2bua::store::{CallStore, InMemoryCallStore, PartitionRole};
use b2bua::turn_probe::{Hold, TurnMark, TurnPoint, TurnProbe};
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup, TurnProbes};
use poll_watchdog::Watchdog;
use repl_net::transport::ReplicationNetwork;
use repl_net_testkit::SimulatedReplicationNetwork;
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use sip_txn::IdGen;
use topology::{Peer, SimulatedMembership};

use super::*;

/// Set in the child's environment: the child test runs only there.
const CHILD: &str = "POLL_WATCHDOG_CHILD";
/// The stuck threshold the child's watchdog runs at.
const THRESHOLD: Duration = Duration::from_millis(200);
/// The bounds of the child worker's exit.
const CHILD_BOUNDS: DrainBounds = DrainBounds {
    grace: Duration::from_secs(1),
    floor: Duration::from_millis(100),
    release_flush: Duration::from_millis(300),
    cdr_flush: Duration::from_millis(100),
};
/// How long the parent waits for the child.
const CHILD_BUDGET: Duration = Duration::from_secs(10);
/// The line the child prints once the backup holds no copy of the call.
const BACKUP_GONE: &str = "backup copy gone";

const W0_SIP: SocketAddr = addr([127, 0, 0, 2], 5080);
const W1_SIP: SocketAddr = addr([127, 0, 0, 4], 5080);
const W0_REPL: SocketAddr = addr([127, 0, 0, 2], 9600);
const W1_REPL: SocketAddr = addr([127, 0, 0, 4], 9600);
const CALLER_AT: SocketAddr = addr([10, 0, 0, 9], 5060);
const CALLEE_AT: SocketAddr = addr([127, 0, 0, 3], 5090);

const fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])), port)
}

/// Run the child test `name` of this binary with `workers` runtime workers,
/// and return what it did.
fn run_child(name: &str, workers: usize) -> Output {
    let exe = std::env::current_exe().expect("the test binary");
    let mut child = Command::new(exe)
        .args([name, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("TOKIO_WORKER_THREADS", workers.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("the child's status reads").is_some() {
            return child.wait_with_output().expect("the child's output reads");
        }
        if started.elapsed() > CHILD_BUDGET {
            let _ = child.kill();
            let out = child.wait_with_output().expect("the child's output reads");
            panic!("the child outlived {CHILD_BUDGET:?}: {}", String::from_utf8_lossy(&out.stdout));
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

/// A call's turn spinning on the CPU ends the process with the watchdog's
/// code, after the call's discharge reached the backup: its copy there is
/// deleted, so no takeover runs the turn's input again.
#[test]
fn a_spinning_call_body_ends_the_process_and_its_discharge_reaches_the_backup() {
    let out = run_child("shutdown::stuck_child_tests::child_spins_a_call_body", 4);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(i32::from(poll_watchdog::EXIT_CODE)), "{all}");
    assert!(all.contains("a poll is stuck on the CPU"), "the watchdog's line: {all}");
    assert!(all.contains("backup_applied"), "the discharge's outcome: {all}");
    assert!(all.contains(BACKUP_GONE), "the backup's copy is deleted: {all}");
}

/// The stuck thread is the runtime's only worker: nothing else of the
/// process runs, and the watchdog thread ends it at its bound.
#[test]
fn a_spinning_call_body_on_the_only_worker_ends_the_process_at_the_hard_bound() {
    let out = run_child("shutdown::stuck_child_tests::child_spins_a_call_body", 1);
    let all = text(&out);
    assert_eq!(out.status.code(), Some(i32::from(poll_watchdog::EXIT_CODE)), "{all}");
    assert!(all.contains("did not exit within"), "the hard exit's line: {all}");
}

/// The child: a primary `w0` and its backup `w1`, a ringing call on `w0`,
/// then one turn of it spun on the CPU. The process leaves through
/// `std::process::exit`, by the stuck exit or by the watchdog's hard bound.
#[test]
#[ignore = "the child process of the stuck-poll tests, run by them"]
fn child_spins_a_call_body() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let log = observe::init_production("stuck-child").expect("the log writer starts");
    let watchdog = Watchdog::process();
    watchdog.start(THRESHOLD).expect("this host reads schedstat");
    let runtime = crate::runtime::runtime().expect("the runtime builds");
    let code = runtime.block_on(async {
        let pair = Pair::start().await;
        let call_ref = pair.ringing_call().await;
        watchdog_serves(watchdog, &pair.w0, CHILD_BOUNDS);
        watchdog.exit_after(CHILD_BOUNDS.release_flush * 2 + CHILD_BOUNDS.cdr_flush);
        tokio::spawn(report_backup_gone(pair.w1_store.clone(), call_ref.clone()));
        pair.spin.armed.store(true, Ordering::SeqCst);
        pair.cancel(&call_ref).await;
        let bounds = CHILD_BOUNDS;
        let exit = Duration::from_secs(30);
        serve_until_stop(&pair.w0, "child", bounds, exit, watchdog, pending(), pending()).await
    });
    // The backup's report rides one more poll of its watcher.
    std::thread::sleep(Duration::from_millis(50));
    drop(log);
    let _ = std::io::stdout().flush();
    let code = if code == ExitCode::from(poll_watchdog::EXIT_CODE) {
        i32::from(poll_watchdog::EXIT_CODE)
    } else {
        1
    };
    std::process::exit(code);
}

/// Print [`BACKUP_GONE`] once `store` held `call_ref`'s copy and then no
/// longer does.
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

/// Spins the first turn of `w0` it sees logged once armed: a call's turn
/// that never leaves the CPU.
#[derive(Default)]
struct Spin {
    armed: AtomicBool,
}

impl TurnProbe for Spin {
    fn reached(&self, mark: &TurnMark<'_>) -> Option<Hold> {
        if mark.point == TurnPoint::Logged && self.armed.swap(false, Ordering::SeqCst) {
            loop {
                std::hint::spin_loop();
            }
        }
        None
    }
}

struct Pair {
    w0: B2buaCore,
    _w1: B2buaCore,
    w1_store: Arc<ReplicatingCallStore>,
    spin: Arc<Spin>,
    caller: Box<dyn UdpEndpoint>,
    _callee: Box<dyn UdpEndpoint>,
}

impl Pair {
    async fn start() -> Pair {
        let clock = Clock::test_at(0);
        let sip = SimulatedSignalingNetwork::new(1);
        let repl = Arc::new(SimulatedReplicationNetwork::with_delay(1));
        let membership = Arc::new(SimulatedMembership::with_clock(Vec::new(), clock.clone()));
        let caller = sip.bind_udp(BindUdpOpts::new(CALLER_AT, 64)).await.expect("caller binds");
        let callee = sip.bind_udp(BindUdpOpts::new(CALLEE_AT, 64)).await.expect("callee binds");
        let spin = Arc::new(Spin::default());
        let probe = TurnProbes::new(spin.clone());
        let parts = Parts { sip: &sip, repl: &repl, membership: &membership, clock: &clock };
        let (w0, _) = parts.node("w0", W0_SIP, W0_REPL, probe).await;
        let (w1, w1_store) = parts.node("w1", W1_SIP, W1_REPL, TurnProbes::default()).await;
        for ordinal in ["w0", "w1"] {
            membership.add(Peer::new(ordinal, ordinal).with_identity(identity(ordinal)));
        }
        let pair = Pair { w0, _w1: w1, w1_store, spin, caller, _callee: callee };
        pair.until("both workers bootstrapped", |p| {
            [&p.w0, &p._w1].iter().all(|w| w.supervisor().is_some_and(|s| s.all_bootstrapped()))
        })
        .await;
        pair
    }

    /// A call on `w0` still being set up, its copy on `w1`: the front proxy's
    /// cookie names the pair.
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
                return call_ref;
            }
            assert!(tokio::time::Instant::now() < deadline, "the backup never held the call");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The caller gives up the call: its CANCEL starts a turn of it on `w0`.
    async fn cancel(&self, call_ref: &str) {
        let cancel = "CANCEL sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-spin\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: spin@10.0.0.9\r\n\
             CSeq: 1 CANCEL\r\n\
             Content-Length: 0\r\n\r\n";
        self.caller.send_to(cancel.as_bytes(), W0_SIP).await.expect("the CANCEL leaves");
        tracing::info!(call_ref, "the caller cancels");
    }

    async fn until(&self, what: &str, done: impl Fn(&Pair) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
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
        turn_probe: TurnProbes,
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
            cdr: Arc::new(InMemoryCdrWriter::new()),
            store: Arc::new(InMemoryCallStore::new()),
            store_faults: Default::default(),
            wire_faults: Default::default(),
            turn_probe,
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
        (B2buaCore::spawn(endpoint, deps), store)
    }
}
