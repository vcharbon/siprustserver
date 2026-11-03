//! **A proxy instance rejoining after it reached no worker.** Instances A and
//! B serve one address, every flow on B. A reads both workers dead at once,
//! as a node that receives nothing from the workers does, so it tells its
//! verdicts to no worker. A call is set up through B and rings. Then A reaches the workers
//! again: it reads one alive, and the flows return to A.
//!
//! The verdicts A took while it reached no worker speak of A's own reach, not
//! of the workers: the first worker A reads alive again returns every worker
//! A reads dead to unknown, for its probe to judge afresh. So the callee's
//! answer to the primary's dialling INVITE, crossing A while A has read only
//! the backup alive again, follows its Via to the primary, as a response
//! bound for a worker the proxy does not read dead does (ADR-0055 §8): the
//! backup takes nothing over, and once A's probe reads the primary alive the
//! call is confirmed and ended by its primary, with one CDR. A request
//! crossing A while it reads the primary unknown goes to the backup, as for a
//! primary not read alive: a caller's CANCEL, which A places by rank among
//! the workers it reads alive, reaches the backup, whose copy takes the call
//! over and ends it.
//!
//! Beyond the failover sweep: a front proxy instance rejoining after it read
//! every worker dead.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use b2bua::repl::SyncLevel;
use call::LimiterEntry;
use call_limiter::{CallStore, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, worker_ordinals, FailoverHarness, Instance, ProxyPair,
    ReplicatedB2buaSut, WorkerHealth, LEASE_OUTLIVING_THE_REPLICA_TTL,
};
use http_net::{HttpServerHandle, HttpTransport};
use scenario_harness::{Agent, ClientInvite, ServerTxn};
use sip_clock::Clock;

use crate::call_terminate_on_backup::{ANSWER, OFFER};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const VIP: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER: &str = "10.0.0.1:8080";

/// Every sender whose flow an ECMP hop spreads over the two instances.
const SENDERS: [&str; 4] = [ALICE, BOB, B1, B2];

fn addr(at: &str) -> SocketAddr {
    at.parse().unwrap()
}

/// Every call to the callee, holding one limiter entry.
fn decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![LimiterEntry { id: "trunk-A".into(), limit: 8 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A ringing call set up through B while A reads both workers dead.
struct Ringing {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    pair: ProxyPair,
    store: Arc<CallStore>,
    _lh: Box<dyn HttpServerHandle>,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    primary_ord: String,
    backup_ord: String,
    call_ref: String,
    call: ClientInvite,
    uas: ServerTxn,
}

impl Ringing {
    fn primary(&self) -> &ReplicatedB2buaSut {
        if self.primary_ord == "b1" {
            &self.w_b1
        } else {
            &self.w_b2
        }
    }

    fn backup(&self) -> &ReplicatedB2buaSut {
        if self.primary_ord == "b1" {
            &self.w_b2
        } else {
            &self.w_b1
        }
    }

    /// A reaches the workers again and reads `first` alive; every flow
    /// returns to A.
    fn rejoin(&self, first: &str) {
        self.pair.a.set_health(first, WorkerHealth::Alive);
        for sender in SENDERS {
            self.pair.assign(addr(sender), Instance::A);
        }
    }

    /// The call is over on both nodes and in the limiter, with one CDR.
    async fn assert_over(&self) {
        let call_ref = &self.call_ref;
        let drained = self
            .fh
            .settle_terminal(async || {
                self.store.stats().current_total == 0
                    && self.w_b1.memory_clean()
                    && self.w_b2.memory_clean()
                    && !self.w_b1.holds_any_trace(call_ref).await
                    && !self.w_b2.holds_any_trace(call_ref).await
            })
            .await;
        assert!(drained, "the call drains on both nodes");
        self.fh.linger_peers(&[&self.alice, &self.bob], Duration::from_secs(3)).await;
        assert_call_fully_over(&[&self.w_b1, &self.w_b2], call_ref, &self.store).await;
    }
}

async fn ringing(name: &str) -> Ringing {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_sync_level(SyncLevel::Applied);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let http = http_net_testkit::SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LEASE_OUTLIVING_THE_REPLICA_TTL, Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let lh: Box<dyn HttpServerHandle> = http.serve(addr(LIMITER), server).await.unwrap();
    let limiter = || -> Arc<dyn CallLimiter> {
        let transport = Arc::new(http.clone());
        Arc::new(HttpCallLimiter::new(transport, addr(LIMITER), Duration::from_millis(150)))
    };
    let pair = fh.spawn_proxy_pair(VIP, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    for sender in SENDERS {
        pair.assign(addr(sender), Instance::B);
    }
    let mut workers = Vec::new();
    for (ordinal, at, peer) in [("b1", B1, "b2"), ("b2", B2, "b1")] {
        let worker = fh
            .spawn_worker_limited(
                ordinal,
                ordinal,
                at,
                &[peer],
                ("127.0.0.1", 5070),
                ("127.0.0.1", 5080),
                decision(),
                limiter(),
            )
            .await;
        workers.push(worker);
    }
    let (w_b2, w_b1) = (workers.pop().unwrap(), workers.pop().unwrap());
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready at steady state");

    pair.a.set_health("b1", WorkerHealth::Dead);
    pair.a.set_health("b2", WorkerHealth::Dead);
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(pair.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, backup_ord) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let call_ref =
        backup.scan_one_backed_up(&primary_ord).await.expect("the backup holds a replica");
    Ringing {
        fh,
        alice,
        bob,
        pair,
        store,
        _lh: lh,
        w_b1,
        w_b2,
        primary_ord,
        backup_ord,
        call_ref,
        call,
        uas,
    }
}

/// A reads the backup alive first; the callee answers across A; A's probe
/// reads the primary alive before the caller's ACK.
#[tokio::test(start_paused = true)]
async fn the_callees_answer_across_a_rejoined_instance_reaches_the_live_primary() {
    let mut r = ringing("rejoin-answer-to-the-primary").await;
    r.rejoin(&r.backup_ord.clone());
    r.uas.respond(200, "OK").with_sdp(ANSWER).await;
    r.call.expect(200).await;
    assert_eq!(r.backup().metrics().repl_takeover_hydrated_total(), 0, "nothing is taken over");
    let primary_ord = r.primary_ord.clone();
    r.pair.a.set_health(&primary_ord, WorkerHealth::Alive);
    let mut dialog = r.call.ack().await;
    r.bob.receive("ACK").await;
    assert!(r.primary().active_calls() == 1, "the primary serves the call");

    let mut bye = dialog.bye().await;
    r.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    r.assert_over().await;
    let cdrs = r.primary().cdr_records().len();
    assert_eq!(cdrs, 1, "the primary writes the call's CDR");
}

/// A reads the backup alive first; the caller cancels across A.
#[tokio::test(start_paused = true)]
async fn the_callers_cancel_across_a_rejoined_instance_ends_the_call() {
    let mut r = ringing("rejoin-cancel").await;
    r.rejoin(&r.backup_ord.clone());
    let mut cancel = r.call.cancel().await;
    r.bob.receive("CANCEL").await.respond(200, "OK").await;
    r.uas.respond(487, "Request Terminated").await;
    cancel.expect(200).await;
    r.call.expect(487).await;
    let primary_ord = r.primary_ord.clone();
    r.pair.a.set_health(&primary_ord, WorkerHealth::Alive);
    r.assert_over().await;
}
