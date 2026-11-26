//! A shard whose inbound queue a flood holds non-empty, as the intake shed
//! holds a real one at its watermark: its loop ends its task poll every
//! cooperative budget of datagrams, so the poll watchdog never reads it stuck,
//! and the other tasks of its thread (the probe server among them) run while
//! the flood lasts.
//!
//! The runtime's poll hooks count the datagrams the shard takes inside each of
//! its polls: the watchdog's reading of a poll, in datagrams instead of CPU
//! time.

use std::cell::Cell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sip_clock::Clock;
use sip_message::CustomParser;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::{Flood, SimulatedSignalingNetwork};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{
    ExternalFaceParts, ForwardAllStrategy, ProxyAddr, ProxyCore, ProxyCoreBuilder, RoutingStrategy,
};
use sip_txn::IdGen;
use tokio::net::UdpSocket;
use tokio::runtime::{Builder, Runtime, TaskMeta};

const PROXY: &str = "192.0.2.1:5060";
const EXTERNAL: &str = "198.51.100.1:5060";
const FLOODER: &str = "192.0.2.40:5060";
const WORKER: &str = "10.0.0.2:5070";
/// The copies of the flood on the paused clock.
const FLOOD: u64 = 4_096;
/// The copies of the flood on the multi-thread runtime: enough for several
/// probe round trips while it lasts.
const LONG_FLOOD: u64 = 8_192;
/// The shard's queue bound, which the flood holds full.
const DEPTH: usize = 64;
/// Tokio's cooperative budget: the ready operations one task poll completes
/// before its next ready one returns pending.
const COOP_BUDGET: u64 = 128;
/// The most datagrams the shard takes between two turns of another task:
/// the current-thread scheduler wakes the tasks a round deferred last first,
/// so the shard may take two polls in a row.
const TURN_GAP: u64 = 2 * COOP_BUDGET;
/// The route guard's bound on a probe's answer.
const PROBE_BOUND: Duration = Duration::from_millis(100);

/// The face of a dual-face shard the flood arrives on.
#[derive(Debug, Clone, Copy)]
enum Flooded {
    Internal,
    External,
}

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("an address")
}

/// RFC 3261 §18.1.2: a response whose top Via names no sent-by of this proxy
/// is discarded. The flood is one such response, the cheapest datagram a
/// shard reads.
fn stray_response() -> Vec<u8> {
    "SIP/2.0 180 Ringing\r\n\
     Via: SIP/2.0/UDP 192.0.2.50:5060;branch=z9hG4bK-stray\r\n\
     From: <sip:alice@192.0.2.20>;tag=alice-tag\r\n\
     To: <sip:bob@192.0.2.30>;tag=bob-tag\r\n\
     Call-ID: stray@192.0.2.20\r\n\
     CSeq: 1 INVITE\r\n\
     Content-Length: 0\r\n\r\n"
        .as_bytes()
        .to_vec()
}

thread_local! {
    /// The copies taken when the task this thread polls entered its poll.
    static TAKEN_AT_ENTRY: Cell<u64> = const { Cell::new(0) };
}

/// The most datagrams of `flood` the shard took inside one of its task polls,
/// read by the runtime's poll hooks on whichever worker polls it.
#[derive(Clone, Default)]
struct LongestPoll {
    flood: Arc<OnceLock<Flood>>,
    shard: Arc<OnceLock<tokio::task::Id>>,
    longest: Arc<AtomicU64>,
}

impl LongestPoll {
    fn taken(&self) -> u64 {
        self.flood.get().map_or(0, Flood::taken)
    }

    fn entered(&self) {
        TAKEN_AT_ENTRY.set(self.taken());
    }

    fn left(&self, task: &TaskMeta<'_>) {
        if self.shard.get() == Some(&task.id()) {
            let inside = self.taken() - TAKEN_AT_ENTRY.get();
            self.longest.fetch_max(inside, Ordering::SeqCst);
        }
    }

    fn runtime(&self, mut builder: Builder) -> Runtime {
        let (before, after) = (self.clone(), self.clone());
        builder
            .enable_all()
            .on_before_task_poll(move |_| before.entered())
            .on_after_task_poll(move |task| after.left(task))
            .build()
            .expect("the runtime builds")
    }

    /// Spawn `core`'s loop over the flood, whose polls this then reads.
    fn spawn(&self, core: ProxyCore, flood: &Flood) -> tokio::task::JoinHandle<()> {
        let _ = self.flood.set(flood.clone());
        let shard = tokio::spawn(core.run());
        let _ = self.shard.set(shard.id());
        shard
    }

    fn longest(&self) -> u64 {
        self.longest.load(Ordering::SeqCst)
    }
}

fn builder() -> ProxyCoreBuilder {
    let worker = ProxyAddr::from(addr(WORKER));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(worker.clone()));
    let registry: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w0", worker)]));
    ProxyCoreBuilder::new(
        ProxyAddr::from(addr(PROXY)),
        strategy,
        registry,
        Arc::new(IdGen::seeded(0xC0FFEE)),
        CustomParser::new(),
    )
    .clock(Clock::test_at(0))
}

async fn bind(net: &SimulatedSignalingNetwork, at: &str) -> Box<dyn UdpEndpoint> {
    net.bind_udp(BindUdpOpts::new(addr(at), DEPTH)).await.expect("bind a face")
}

fn flood_over(endpoint: Box<dyn UdpEndpoint>, total: u64) -> (Box<dyn UdpEndpoint>, Flood) {
    Flood::over(endpoint, &stray_response(), addr(FLOODER), total)
}

/// A single-face shard read ahead of a flood of `total` stray responses.
async fn single_face(net: &SimulatedSignalingNetwork, total: u64) -> (ProxyCore, Flood) {
    let (endpoint, flood) = flood_over(bind(net, PROXY).await, total);
    (builder().build(endpoint), flood)
}

/// A dual-face shard whose `flooded` face is read ahead of the flood.
async fn dual_face(net: &SimulatedSignalingNetwork, flooded: Flooded) -> (ProxyCore, Flood) {
    let (internal, external) = (bind(net, PROXY).await, bind(net, EXTERNAL).await);
    let (internal, external, flood) = match flooded {
        Flooded::Internal => {
            let (internal, flood) = flood_over(internal, FLOOD);
            (internal, external, flood)
        }
        Flooded::External => {
            let (external, flood) = flood_over(external, FLOOD);
            (internal, external, flood)
        }
    };
    let external =
        ExternalFaceParts { endpoint: external, advertised: ProxyAddr::from(addr(EXTERNAL)) };
    (builder().external_face(external).build(internal), flood)
}

/// A task, started with the flood, that takes a turn whenever the thread
/// lets it until the flood is over: its turns, and the most datagrams the
/// shard took before its first turn or between two of them.
async fn turns_while(flood: Flood) -> (u64, u64) {
    let (mut turns, mut widest_gap, mut last) = (0, 0, 0);
    loop {
        let now = flood.taken();
        widest_gap = widest_gap.max(now - last);
        if !flood.lasts() {
            return (turns, widest_gap);
        }
        (last, turns) = (now, turns + 1);
        tokio::task::yield_now().await;
    }
}

/// On the paused current-thread runtime: the shard's longest poll, and the
/// widest gap between two turns of a sibling task, over the flood.
fn flood_on_one_thread(shard: impl AsyncFnOnce(&SimulatedSignalingNetwork) -> (ProxyCore, Flood)) {
    let longest = LongestPoll::default();
    let mut on_one_thread = Builder::new_current_thread();
    on_one_thread.start_paused(true);
    longest.runtime(on_one_thread).block_on(async {
        let net = SimulatedSignalingNetwork::new(1);
        let (core, flood) = shard(&net).await;
        let shard = longest.spawn(core, &flood);

        flood.start(DEPTH);
        let (turns, widest_gap) = tokio::spawn(turns_while(flood)).await.expect("the task ends");
        shard.abort();

        assert!(
            widest_gap <= TURN_GAP,
            "another task of the thread runs within two budgets of datagrams \
             ({widest_gap} taken between two of its turns, {turns} turns)"
        );
    });
    assert!(
        longest.longest() <= COOP_BUDGET,
        "no poll of the shard takes more than a budget of datagrams ({} of {FLOOD})",
        longest.longest()
    );
}

#[test]
fn a_flooded_shard_ends_each_poll_within_a_budget_and_its_thread_runs_on() {
    flood_on_one_thread(async |net| single_face(net, FLOOD).await);
}

#[test]
fn a_dual_face_shard_flooded_on_either_face_ends_each_poll_within_a_budget() {
    for flooded in [Flooded::Internal, Flooded::External] {
        flood_on_one_thread(async move |net| dual_face(net, flooded).await);
    }
}

/// A socket round trip answered by a sibling task, repeated while the flood
/// lasts, as the route guard reads the probe server: the round trips and the
/// slowest one, `None` once one went unanswered past [`PROBE_BOUND`].
async fn probes_while(flood: Flood) -> (u32, Option<Duration>) {
    let localhost = SocketAddr::from(([127, 0, 0, 1], 0));
    let echo = UdpSocket::bind(localhost).await.expect("bind the echo");
    let prober = UdpSocket::bind(localhost).await.expect("bind the prober");
    prober.connect(echo.local_addr().expect("the echo's address")).await.expect("connect");
    tokio::spawn(async move {
        let mut buf = [0; 16];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let (mut trips, mut slowest) = (0, Duration::ZERO);
    while flood.lasts() {
        let sent = tokio::time::Instant::now();
        prober.send(b"probe").await.expect("send a probe");
        let mut buf = [0; 16];
        if tokio::time::timeout(PROBE_BOUND, prober.recv(&mut buf)).await.is_err() {
            return (trips, None);
        }
        (trips, slowest) = (trips + 1, slowest.max(sent.elapsed()));
    }
    (trips, Some(slowest))
}

#[test]
fn a_flooded_shard_on_a_multi_thread_runtime_ends_each_poll_within_a_budget() {
    let longest = LongestPoll::default();
    let mut on_two_workers = Builder::new_multi_thread();
    on_two_workers.worker_threads(2);
    longest.runtime(on_two_workers).block_on(async {
        let net = SimulatedSignalingNetwork::new(1);
        let (core, flood) = single_face(&net, LONG_FLOOD).await;
        let shard = longest.spawn(core, &flood);

        let probes = tokio::spawn(probes_while(flood.clone()));
        flood.start(DEPTH);
        let (trips, slowest) = probes.await.expect("the prober ends");
        shard.abort();

        assert!(
            trips > 0 && slowest.is_some(),
            "a probe is answered within {PROBE_BOUND:?} throughout the flood \
             ({trips} answered, the slowest {slowest:?})"
        );
    });
    assert!(
        longest.longest() <= COOP_BUDGET,
        "no poll of the shard takes more than a budget of datagrams ({} of {LONG_FLOOD})",
        longest.longest()
    );
}
