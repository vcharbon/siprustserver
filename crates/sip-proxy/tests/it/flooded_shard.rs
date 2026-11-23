//! A shard whose inbound queue a flood holds non-empty, as the intake shed
//! holds a real one at its watermark: its loop ends its task poll every
//! cooperative budget of datagrams, so the poll watchdog never reads it stuck,
//! and the other tasks of its thread (the probe server among them) run while
//! the flood lasts.
//!
//! The runtime is the paused current-thread one, its poll hooks counting the
//! datagrams the shard takes inside each poll: the watchdog's reading of a
//! poll, in datagrams instead of CPU time.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use sip_clock::Clock;
use sip_message::CustomParser;
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::{Flood, SimulatedSignalingNetwork};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::{ForwardAllStrategy, ProxyAddr, ProxyCoreBuilder, RoutingStrategy};
use sip_txn::IdGen;

const PROXY: &str = "192.0.2.1:5060";
const FLOODER: &str = "192.0.2.40:5060";
const WORKER: &str = "10.0.0.2:5070";
/// The copies of the flood.
const FLOOD: u64 = 4_096;
/// The shard's queue bound, which the flood holds full.
const DEPTH: usize = 64;
/// Tokio's cooperative budget: the ready operations one task poll completes
/// before its next ready one returns pending.
const COOP_BUDGET: u64 = 128;
/// The most datagrams the shard takes between two turns of another task:
/// the current-thread scheduler wakes the tasks a round deferred last first,
/// so the shard may take two polls in a row.
const TURN_GAP: u64 = 2 * COOP_BUDGET;

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

/// The most datagrams of `flood` the shard took inside one task poll, read by
/// the runtime's poll hooks.
#[derive(Clone, Default)]
struct LongestPoll {
    flood: Arc<OnceLock<Flood>>,
    at_entry: Arc<AtomicU64>,
    longest: Arc<AtomicU64>,
}

impl LongestPoll {
    fn taken(&self) -> u64 {
        self.flood.get().map_or(0, Flood::taken)
    }

    fn runtime(&self) -> tokio::runtime::Runtime {
        let (before, after) = (self.clone(), self.clone());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .on_before_task_poll(move |_| before.at_entry.store(before.taken(), Ordering::SeqCst))
            .on_after_task_poll(move |_| {
                let inside = after.taken() - after.at_entry.load(Ordering::SeqCst);
                after.longest.fetch_max(inside, Ordering::SeqCst);
            })
            .build()
            .expect("the runtime builds")
    }
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

#[test]
fn a_flooded_shard_ends_each_poll_within_a_budget_and_its_thread_runs_on() {
    let longest = LongestPoll::default();
    longest.runtime().block_on(async {
        let net = SimulatedSignalingNetwork::new(1);
        let endpoint =
            net.bind_udp(BindUdpOpts::new(addr(PROXY), DEPTH)).await.expect("bind the proxy");
        let (endpoint, flood) = Flood::over(endpoint, &stray_response(), addr(FLOODER), FLOOD);
        let _ = longest.flood.set(flood.clone());
        let worker = ProxyAddr::from(addr(WORKER));
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(worker.clone()));
        let registry: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w0", worker)]));
        let core = ProxyCoreBuilder::new(
            ProxyAddr::from(addr(PROXY)),
            strategy,
            registry,
            Arc::new(IdGen::seeded(0x1193)),
            CustomParser::new(),
        )
        .clock(Clock::test_at(0))
        .build(endpoint);
        let shard = tokio::spawn(core.run());

        flood.start(DEPTH);
        let (turns, widest_gap) = tokio::spawn(turns_while(flood)).await.expect("the task ends");
        shard.abort();

        assert!(
            widest_gap <= TURN_GAP,
            "another task of the thread runs within two budgets of datagrams \
             ({widest_gap} taken between two of its turns, {turns} turns)"
        );
    });
    let longest = longest.longest.load(Ordering::SeqCst);
    assert!(
        longest <= COOP_BUDGET,
        "no poll of the shard takes more than a budget of datagrams ({longest} of {FLOOD})"
    );
}
