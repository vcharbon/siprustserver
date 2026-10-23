//! **Every leg of a call names the backup that holds its replica (ADR-0055
//! §8).** Three workers. The caller's INVITE lands on `b1` with `b3` as its
//! backup; the call's replica lives on `b3`. By the time `b1` dials the callee,
//! `b3` refuses new calls, so the proxy's own pick of a backup for the callee's
//! dialog would be `b2`, a node that holds nothing of the call. The worker
//! states the call's backup on its INVITE and the proxy signs that into the
//! callee's dialog: the callee's dialog names `b3` too.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ b1 (primary)     backup b3 holds the replica
//!                   b3 refuses new calls
//!   b1 ─INVITE─▶ proxy ─▶ bob                 the callee's dialog names b3
//!   bob ─180─▶ b1 ─▶ alice; the ringing call replicates to b3
//!               ✗ b1 killed ✗
//!   bob ─200─▶ proxy ─▶ b3 (takes the call over) ─200─▶ alice ─ACK─▶ b3 ─ACK─▶ bob
//!   alice ─BYE─▶ b3 ─BYE─▶ bob; b1 reboots and discharges the call: one CDR
//! ```

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Acceptance, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const WORKERS: [(&str, &str); 3] =
    [("b1", "127.0.0.1:5091"), ("b2", "127.0.0.1:5092"), ("b3", "127.0.0.1:5093")];

/// How long the routing decision takes: the window in which `b3` stops
/// accepting new calls, between the caller's INVITE and the callee's.
const DECISION_DELAY: Duration = Duration::from_millis(300);
/// Long enough for a write to reach the backup.
const REPLICATE: Duration = Duration::from_millis(500);

/// A decision engine that answers `new_call` after [`DECISION_DELAY`].
struct SlowDecision(ScriptedDecisionEngine);

#[async_trait]
impl CallDecisionEngine for SlowDecision {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(DECISION_DELAY).await;
        self.0.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.0.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.0.call_refer(req).await
    }
}

#[tokio::test(start_paused = true)]
async fn the_callees_answer_after_the_primarys_death_reaches_the_replica_holder() {
    let mut fh = FailoverHarness::new("one-backup-on-every-leg", &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let pool: Vec<(&str, std::net::SocketAddr)> =
        WORKERS.iter().map(|(o, a)| (*o, a.parse().unwrap())).collect();
    let proxy = fh.spawn_proxy(PROXY, &pool).await;
    let mut workers: Vec<ReplicatedB2buaSut> = Vec::new();
    for (ordinal, sip) in WORKERS {
        let peers: Vec<&str> = WORKERS.iter().map(|(o, _)| *o).filter(|o| *o != ordinal).collect();
        let decision: Arc<dyn CallDecisionEngine> =
            Arc::new(SlowDecision(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)));
        let worker = fh
            .spawn_worker_limited(
                ordinal,
                ordinal,
                sip,
                &peers,
                ("127.0.0.1", 5070),
                ("127.0.0.1", 5080),
                decision,
                Arc::new(NoopLimiter),
            )
            .await;
        workers.push(worker);
    }
    fh.advance(Duration::from_millis(500)).await;
    assert!(workers.iter().all(|w| w.is_ready()), "every worker ready at steady state");

    // ── The caller's INVITE lands on b1, backed up by b3 ─────────────────────
    let placed = |id: &str| proxy.placement_of(id).is_some_and(|(p, b)| p == "b1" && b == "b3");
    alice.seed_next_call_id(placed);
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;

    // ── b3 stops taking new calls before b1 dials the callee ─────────────────
    proxy.set_acceptance("b3", Acceptance::Refuse);
    let mut uas = bob.receive("INVITE").await;
    let (pri, bak) = worker_ordinals(uas.request());
    assert_eq!(
        (pri.as_str(), bak.as_str()),
        ("b1", "b3"),
        "the callee's dialog names the call's backup, the one holding its replica"
    );
    proxy.set_acceptance("b3", Acceptance::Accept);
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(REPLICATE).await;
    let call_ref =
        workers[2].scan_one_backed_up("b1").await.expect("b3 holds the ringing call's replica");
    assert!(workers[1].scan_one_backed_up("b1").await.is_none(), "b2 holds nothing of it");

    // ── b1 dies; the callee answers ─────────────────────────────────────────
    fh.mark("b1", None, "crash", "primary down with the callee ringing");
    let hydrated_before = workers[2].metrics().repl_takeover_hydrated_total();
    workers[0].crash();
    proxy.set_health("b1", WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    assert_eq!(
        workers[2].metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the callee's 200 took the call over on b3"
    );
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── The call ends on b3; b1 returns and discharges it ───────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(5)).await;
    fh.mark("b1", None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = workers[0].reboot().await;
    proxy.set_address("b1", new_addr);
    fh.note_worker_rebound("b1", new_addr);
    workers[1].simulate_peer_added("b1");
    workers[2].simulate_peer_added("b1");
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if workers[0].is_ready() {
            break;
        }
    }
    assert!(workers[0].is_ready(), "the rebooted b1 is ready");
    proxy.set_health("b1", WorkerHealth::Alive);

    let nodes: Vec<&ReplicatedB2buaSut> = workers.iter().collect();
    let drained = fh
        .settle_terminal(async || {
            for n in &nodes {
                if !n.memory_clean() || n.holds_any_trace(&call_ref).await {
                    return false;
                }
            }
            true
        })
        .await;
    assert!(drained, "the cluster releases the call");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes, &call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&nodes, &call_ref).await;
    drop((workers, proxy));
}
