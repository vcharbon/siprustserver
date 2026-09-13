//! **A member that is not ready stays a replication peer.**
//!
//! A worker whose readiness probe fails — or whose container restarts — keeps
//! its endpoint in the slice at the same address with `ready=false` (ADR-0031
//! case 4). Routing and replication read that one snapshot with two predicates:
//! the proxy departs the ordinal (out of the projection, address tombstoned
//! `Dead`) while its peers keep pulling it on presence alone (D1).
//!
//! ```text
//!   ringing · elder serves · survivor holds the ringing replica
//!   t0        the elder's endpoint flaps not ready (same address, in the slice)
//!             proxy: ordinal gone, address Dead · survivor: peer kept, not parked
//!   +3 s      the elder's ring deadline fires: 480 to the caller, CANCEL to the callee
//!             the terminal state it authors reaches the survivor's replica
//!   +4 s      the endpoint is ready again: proxy Unknown → Alive; the link is
//!             plain active on the same puller (watermark continuous)
//!   then      a second call through the pool, answered and hung up cleanly
//! ```
//!
//! What the flap must not do is park the peer: a parked peer sees nothing the
//! worker authors while it is unroutable, and the ring deadline it fires would
//! exist on one node only. The caller takes exactly one final.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use call::{CallBodyCodec, LegState, MsgpackCodec, TimerType};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Belief, FailoverHarness,
    PartitionRole, PeerLink, ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::Agent;
use sip_message::SipMessage;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;

/// A short ring deadline, so the elder's own timer fires inside the flap.
const NO_ANSWER_SEC: i64 = 3;

/// A failover-capable route arming the per-b-leg `NoAnswer`, whose failure
/// consult answers with a `480` reject.
fn no_answer_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("not-ready-member-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                CallTreatment::Reject(RejectDecision {
                    reject_code: 480,
                    reject_reason: Some("Temporarily Unavailable".into()),
                    update_headers: None,
                })
            })
            .build(),
    )
}

/// The first replicated ref in `bak:{primary}` on the survivor.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the survivor");
}

/// The survivor's replica, decoded (`None` once the replica is gone).
async fn replica(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> Option<call::Call> {
    let body = backup.get(BAK, primary, call_ref).await?;
    Some(MsgpackCodec::new().decode(&body).expect("replica body decodes"))
}

/// The absolute deadline of the survivor's replica per-b-leg `NoAnswer` entry.
async fn ring_deadline(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> i64 {
    let call = replica(backup, primary, call_ref).await.expect("replica body present");
    assert_ne!(call.a_leg.state, LegState::Confirmed, "the survivor holds the ringing a-leg");
    call.timers
        .iter()
        .find(|t| t.timer_type == TimerType::NoAnswer)
        .map(|t| t.fire_at)
        .expect("the replica carries the route-time NoAnswer")
}

/// Whether the survivor's view of the call is over: the replica is gone, or its
/// a-leg is terminated.
async fn replica_terminal(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> bool {
    match replica(backup, primary, call_ref).await {
        None => true,
        Some(c) => c.a_leg.state == LegState::Terminated,
    }
}

/// Pump until no node holds a trace of `call_ref`. Answers keepalive OPTIONS
/// along the way.
async fn settle_released(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) {
    let released = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(40), async || {
            if let Some(mut t) = alice.try_receive_tolerating("OPTIONS", &[]).await {
                t.respond(200, "OK").await;
            }
            if let Some(mut t) = bob.try_receive_tolerating("OPTIONS", &[]).await {
                t.respond(200, "OK").await;
            }
            let mut clean = true;
            for n in nodes {
                if n.holds_any_trace(call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(released, "every node released {call_ref} within Timer H of the teardown");
}

fn report_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports")
        .join("not-ready-member-stays-pulled")
}

#[tokio::test(start_paused = true)]
async fn a_member_flapping_not_ready_is_departed_by_the_proxy_and_still_pulled() {
    let name = "not-ready-member-stays-pulled";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    let w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let (elder, survivor): (&ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let fire_at = ring_deadline(survivor, &pri_ord, &call_ref).await;
    assert!(elder.serves(&call_ref), "the primary serves the ringing call");
    let elder_addr: SocketAddr = elder.sip_addr();
    let w_before = survivor.backup_flow_watermark(&pri_ord);
    assert_eq!(survivor.peer_link(&pri_ord), PeerLink::Active);

    // ── t0: the elder's endpoint flaps not ready at the same address ──────────
    fh.flap_not_ready(&pri_ord);
    fh.advance(Duration::from_millis(200)).await;
    assert!(proxy.health(&pri_ord).is_none(), "the not-ready ordinal is out of the projection");
    assert_eq!(
        proxy.health_at(elder_addr),
        Some(WorkerHealth::Dead),
        "its address is tombstoned for Timer H"
    );
    assert_eq!(
        survivor.peer_link(&pri_ord),
        PeerLink::Kept,
        "the survivor keeps pulling the member on presence alone"
    );
    assert!(elder.serves(&call_ref), "the process is untouched and still serves its call");

    // ── the elder's ring deadline fires inside the flap ──────────────────────
    // The elder authors the 480 to the caller and the CANCEL to the callee; the
    // callee answers the CANCEL and terminates its INVITE (RFC 3261 §9.2). The
    // callee's responses travel the proxy's response path, where the elder's
    // address reads `Dead`, so they reverse-fail to the survivor.
    assert!(fh.now_ms() < fire_at, "the flap precedes the ring deadline");
    let mut finals_to_alice: Vec<u16> = Vec::new();
    // The callee answers the CANCEL and terminates its INVITE the instant the
    // CANCEL lands (the blocking receive advances the clock only to its
    // delivery), so the reverse-failed 487 races the elder's terminal flush as
    // closely as the fabric's transit delay allows.
    fh.advance(Duration::from_millis((fire_at - fh.now_ms()).max(0) as u64)).await;
    let mut cancel = bob.receive_tolerating("CANCEL", &["OPTIONS", "ACK"]).await;
    let mut cancels_to_bob = 1usize;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    let deadline_seen = fh.now_ms();
    while fh.now_ms() < deadline_seen + 2_000 {
        fh.advance(Duration::from_millis(200)).await;
        while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS", "ACK"]).await
        {
            cancels_to_bob += 1;
            cancel.respond(200, "OK").await;
        }
        while let Some(msg) = alice.take_queued().await {
            match msg {
                SipMessage::Response(r) if r.cseq().method().as_str() == "INVITE" => {
                    if r.status() >= 300 {
                        call.ack_non_2xx(&r).await.expect("the caller hop-ACKs the final");
                    }
                    finals_to_alice.push(r.status());
                }
                SipMessage::Response(_) => {}
                SipMessage::Request(r) => panic!("unexpected {} toward alice", r.method()),
            }
        }
    }
    fh.mark(
        &pri_ord,
        None,
        "deadline",
        &format!(
            "finals to the caller {finals_to_alice:?}; CANCELs to the callee {cancels_to_bob}"
        ),
    );
    assert_eq!(finals_to_alice, vec![480], "the caller takes exactly one final, the elder's 480");
    assert!(
        cancels_to_bob <= 2,
        "the callee's ringing INVITE takes one CANCEL, at most retransmitted once: {cancels_to_bob}"
    );

    // The terminal state the elder authored while unroutable reached the
    // survivor: the replica is terminated or already deleted.
    let terminal = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(2), async || {
            replica_terminal(survivor, &pri_ord, &call_ref).await
        })
        .await;
    assert!(terminal, "the survivor's replica carries the elder's terminal state (ADR-0031 D1)");
    assert_eq!(survivor.peer_link(&pri_ord), PeerLink::Kept, "still kept, never parked");

    // ── the endpoint is ready again at the same address ──────────────────────
    fh.flap_ready(&pri_ord);
    fh.advance(Duration::from_millis(200)).await;
    assert_eq!(
        proxy.health(&pri_ord),
        Some(WorkerHealth::Unknown),
        "the returning endpoint awaits its probe"
    );
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(survivor.peer_link(&pri_ord), PeerLink::Active, "a plain active peer again");
    let w_after = survivor.backup_flow_watermark(&pri_ord);
    assert_eq!(w_after.gen, w_before.gen, "the same puller, the same incarnation");
    assert!(
        w_after.counter > w_before.counter,
        "the puller advanced across the flap without a respawn: {w_before:?} → {w_after:?}"
    );

    // ── a second call through the pool, answered and hung up ────────────────
    let mut call2 = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas2 = bob.receive_tolerating("INVITE", &["OPTIONS", "ACK"]).await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    let mut dialog2 = call2.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    scenario_harness::callflow::hangup(&mut dialog2, &bob).await;

    // ── the cluster releases both calls ──────────────────────────────────────
    let nodes: [&ReplicatedB2buaSut; 2] = [elder, survivor];
    settle_released(&fh, &alice, &bob, &nodes[..], &call_ref).await;
    let drained =
        fh.settle_terminal(async || nodes[0].memory_clean() && nodes[1].memory_clean()).await;
    assert!(drained, "the cluster drains the second call within the settle budget");
    assert_call_fully_released(&nodes[..], &call_ref).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR for the rejected call");
    let cdrs: usize = nodes.iter().map(|n| n.cdr_records().len()).sum();
    assert_eq!(cdrs, 2, "one CDR per call across the cluster, the second call included");

    // ── what the ledger recorded ─────────────────────────────────────────────
    // Nothing — primitive or sampler — ever parked the peer; the live link reads
    // above are the proof that it was kept through the flap.
    let ledger = fh.view_ledger();
    let survivor_view = ledger.beliefs(&format!("{bak_ord}#g1"), &pri_ord);
    assert!(!survivor_view.contains(&Belief::PeerParked), "never parked: {survivor_view:?}");
    assert_eq!(survivor_view.last(), Some(&Belief::PeerActive), "{survivor_view:?}");
    let proxy_view = ledger.beliefs("proxy", &pri_ord);
    assert!(proxy_view.contains(&Belief::Absent), "the proxy departed it: {proxy_view:?}");
    assert_eq!(
        proxy_view.last(),
        Some(&Belief::Registered(WorkerHealth::Alive)),
        "and holds it alive again: {proxy_view:?}"
    );
    assert_eq!(
        ledger.beliefs(&format!("{pri_ord}#g1"), &pri_ord),
        vec![Belief::Running { gen: 1 }],
        "the flapping member's process never changed"
    );

    let written = fh
        .write_unified_report(
            &report_dir(),
            "report",
            "A member flapping not ready is departed by the proxy and still pulled",
            true,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());

    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}
