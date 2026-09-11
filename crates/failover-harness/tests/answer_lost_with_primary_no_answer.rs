//! **A restored `NoAnswer` re-answers a call the caller already ACKed
//!.**
//!
//! The record a reboot reclaim materialises can read a call as still ringing
//! — a-leg unanswered, the per-b-leg `NoAnswer` armed at its ORIGINAL absolute
//! deadline — while the caller already holds the B2BUA's 2xx and has ACKed it.
//! At that deadline the `no-answer` treatment authors a second final on the
//! caller's INVITE server transaction (RFC 3261 §17.2.1: one final per INVITE
//! server transaction) and tears the established call down. Two ways there:
//!
//! - the primary answered and died before the answer version reached the
//!   backup (`an_answer_the_primary_never_replicated_…`): the backup's replica
//!   is rewound to the ringing body it genuinely held before the answer;
//! - the callee answered while the primary was dead, so the SURVIVOR's takeover
//!   copy answered the caller, and the rebooted primary reclaimed the ringing
//!   body (`an_answer_served_by_the_takeover_copy_…`) — the endurance timeline.
//!
//! Unlike the 059 pin (`stale_no_answer_reclaim.rs`, a Confirmed replica with a
//! stale timer), both restored copies read the caller as unanswered.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use call::{parse_call_ref, CallBodyCodec, LegState, MsgpackCodec, TimerType};
use failover_harness::{
    worker_ordinals, FailoverHarness, PartitionRole, ProxySut, ReplicatedB2buaSut, WorkerHealth,
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

/// The production ring deadline (`ROUTING_MOCK_NO_ANSWER_MS=30000`): armed at
/// route time, far enough out for reboot + ready + bulk reclaim to complete
/// before it fires on the restored copy.
const NO_ANSWER_SEC: i64 = 30;

/// The production shape: a failover-capable route (callback context) arming
/// the per-b-leg `NoAnswer`, whose `no_answer_timeout` failure consult the
/// engine answers with the routing-mock's `480` REJECT.
fn no_answer_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("078-no-answer-ctx".into());
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

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the proxy, both ready.
async fn spawn_cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane)); // lanes registered for reporting only

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
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// The first replicated ref in `bak:{primary}` on the backup.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            if let Some(p) = parse_call_ref(&rf) {
                assert_eq!(p.primary, primary, "backed-up ref encodes its primary");
            }
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the backup");
}

/// The replica body the backup holds for `call_ref`: raw and decoded.
async fn replica(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> (Vec<u8>, call::Call) {
    let body = backup.get(BAK, primary, call_ref).await.expect("replica body present");
    let call = MsgpackCodec::new().decode(&body).expect("replica body decodes");
    (body, call)
}

/// The absolute deadline of the replica's per-b-leg `NoAnswer` entry.
fn no_answer_deadline(call: &call::Call) -> i64 {
    call.timers
        .iter()
        .find(|t| t.timer_type == TimerType::NoAnswer)
        .map(|t| t.fire_at)
        .expect("the replica carries the route-time NoAnswer")
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP, drive it
/// ready and let the bulk reclaim run. With `announce`, the proxy learns the
/// new address and reads the worker alive; without it, the proxy's view lags
/// the reclaim (old address, still dead — the k8s endpoint + readiness lag),
/// so in-dialog traffic keeps failing over to the survivor. Returns the new
/// address for a later [`announce_rebooted`].
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
    announce: bool,
) -> std::net::SocketAddr {
    let new_addr = primary.reboot().await;
    if announce {
        announce_rebooted(fh, primary_ord, proxy, new_addr);
    }
    survivor.simulate_peer_added(primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed)
    new_addr
}

/// The proxy learns the rebooted primary's new address and reads it alive.
fn announce_rebooted(
    fh: &mut FailoverHarness,
    primary_ord: &str,
    proxy: &ProxySut,
    new_addr: std::net::SocketAddr,
) {
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    proxy.set_health(primary_ord, WorkerHealth::Alive);
}

/// What the restored deadline did on the wire: the first INVITE final alice
/// took after her 2xx, and the CANCELs bob's answered INVITE drew.
struct DeadlineCrossing {
    stray_final: Option<u16>,
    cancels_to_bob: usize,
}

/// Cross the restored `NoAnswer` deadline. Bob's INVITE transaction took its
/// 2xx: a CANCEL reaching it draws a 481 (RFC 3261 §9.2). Alice's INVITE
/// transaction took its 2xx too: any INVITE final reaching her now is a second
/// final (§17.2.1).
async fn cross_deadline(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    fire_at: i64,
) -> DeadlineCrossing {
    let mut out = DeadlineCrossing { stray_final: None, cancels_to_bob: 0 };
    while fh.now_ms() < fire_at + 2_000 {
        fh.advance(Duration::from_secs(1)).await;
        while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS"]).await {
            out.cancels_to_bob += 1;
            cancel.respond(481, "Call/Transaction Does Not Exist").await;
        }
        while let Some(msg) = alice.take_queued().await {
            match msg {
                SipMessage::Response(r) if r.cseq().method().as_str() == "INVITE" => {
                    out.stray_final.get_or_insert(r.status());
                }
                SipMessage::Response(_) => {}
                SipMessage::Request(r) => panic!("unexpected {} toward alice", r.method()),
            }
        }
    }
    out
}

/// Pump until no node holds a trace of `call_ref` — a survivor's late takeover
/// copy self-releases at Timer H (32 s past the 2xx) — then assert the
/// cluster-wide release. Answers keepalive OPTIONS along the way.
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
    failover_harness::assert_call_fully_released(nodes, call_ref).await;
}

/// The live copy `node` serves for `call_ref` reads the caller as answered and
/// carries no ring deadline.
fn assert_answered_on(node: &ReplicatedB2buaSut, call_ref: &str, what: &str) {
    let live = node
        .live_call(call_ref)
        .unwrap_or_else(|| panic!("{what}: {} serves the call", node.ordinal()));
    assert_eq!(live.a_leg.state, LegState::Confirmed, "{what}: a-leg Confirmed");
    assert!(
        !live.timers.iter().any(|t| t.timer_type == TimerType::NoAnswer),
        "{what}: the folded ledger carries no NoAnswer",
    );
}

fn report_dir(stem: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports").join(stem)
}

/// The primary answered alice and died before the answer version reached the
/// backup; the rebooted primary reclaims the ringing body. The restored
/// `NoAnswer` must not re-answer the call: no second final on alice's INVITE
/// server transaction, no CANCEL on bob's answered one, and the call stays
/// serviceable end-to-end.
#[tokio::test(start_paused = true)]
#[ignore = "open half: the answer version died with the primary, so no copy can \
            know the caller was answered — needs an answer durable before the 2xx leaves, or a \
            wire check at the ring deadline (ADR-0014 rejects sync confirm-replicate)"]
async fn an_answer_the_primary_never_replicated_draws_no_second_final_from_no_answer() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("078-answer-lost-with-primary-no-answer").await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    let (ringing_body, ringing) = replica(b2, &primary_ord, &call_ref).await;
    assert_ne!(ringing.a_leg.state, LegState::Confirmed, "the backup holds the ringing a-leg");
    let fire_at = no_answer_deadline(&ringing);

    // ── the answer: 2xx to alice, alice ACKs, bob is ACKed — all on the primary
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.serves(&call_ref), "the primary serves the established call");

    // ── the primary dies; the answer version never reached the backup ───────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.mark(
        &bak_ord,
        Some(&primary_ord),
        "replication lag",
        "answer version lost with the primary",
    );
    b2.rewind_replica(BAK, &primary_ord, &call_ref, ringing_body).await;
    let (_, stale) = replica(b2, &primary_ord, &call_ref).await;
    assert_ne!(
        stale.a_leg.state,
        LegState::Confirmed,
        "the backup still reads the call as ringing"
    );
    assert_eq!(
        no_answer_deadline(&stale),
        fire_at,
        "the stale copy still arms the route-time NoAnswer"
    );
    fh.advance(Duration::from_secs(1)).await;

    // ── reboot → bootstrap re-hydrates pri:{self} → REAL bulk reclaim ────────
    reboot_and_reclaim(&mut fh, b1, b2, &primary_ord, &proxy, true).await;
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed the call");
    assert!(fh.now_ms() < fire_at, "the reclaim completes before the ring deadline");

    let crossed = cross_deadline(&fh, &alice, &bob, fire_at).await;
    let passed = crossed.stray_final.is_none() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(
            &report_dir("078-answer-lost-with-primary-no-answer"),
            "report",
            "078 — the answer the primary never replicated",
            passed,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());
    assert_eq!(
        crossed.stray_final, None,
        "a second final on alice's already-answered INVITE server transaction (RFC 3261 §17.2.1)",
    );
    assert_eq!(crossed.cancels_to_bob, 0, "bob's answered INVITE takes no CANCEL");

    // ── the surviving call terminates normally: BYE end-to-end ───────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle_released(&fh, &alice, &bob, &[&*b1, &*b2], &call_ref).await;

    drop((w_b1, w_b2, proxy));
}

/// The endurance timeline: the callee answers while the primary is dead, so
/// the survivor's takeover copy answers alice; the primary reboots and reclaims
/// the ringing body it was backed up with. The restored `NoAnswer` must not
/// re-answer the call the survivor established.
#[tokio::test(start_paused = true)]
async fn an_answer_served_by_the_takeover_copy_draws_no_second_final_from_no_answer() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("078-answered-on-backup-then-reclaim").await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    let (_, ringing) = replica(b2, &primary_ord, &call_ref).await;
    let fire_at = no_answer_deadline(&ringing);

    // ── the primary dies mid-ring ────────────────────────────────────────────
    fh.mark(&primary_ord, None, "crash", "primary down while the callee rings");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;
    let hydrated_before = b2.metrics().repl_takeover_hydrated_total();

    // ── the callee answers: the survivor's takeover copy answers alice ───────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the callee's 2xx hydrated the call onto the survivor (takeover fired)",
    );
    assert!(b2.serves(&call_ref), "the survivor serves the established call");

    // ── the primary returns and reclaims ─────────────────────────────────────
    reboot_and_reclaim(&mut fh, b1, b2, &primary_ord, &proxy, true).await;
    assert!(fh.now_ms() < fire_at, "the reclaim completes before the ring deadline");
    fh.mark(
        &primary_ord,
        None,
        "after reclaim",
        &format!(
            "primary serves={} survivor serves={}",
            b1.serves(&call_ref),
            b2.serves(&call_ref)
        ),
    );

    let crossed = cross_deadline(&fh, &alice, &bob, fire_at).await;
    let passed = crossed.stray_final.is_none() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(
            &report_dir("078-answered-on-backup-then-reclaim"),
            "report",
            "078 — the answer served by the takeover copy",
            passed,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());
    assert_eq!(
        crossed.stray_final, None,
        "a second final on alice's already-answered INVITE server transaction (RFC 3261 §17.2.1)",
    );
    assert_eq!(crossed.cancels_to_bob, 0, "bob's answered INVITE takes no CANCEL");

    // ── the surviving call terminates normally: BYE end-to-end ───────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle_released(&fh, &alice, &bob, &[&*b1, &*b2], &call_ref).await;

    drop((w_b1, w_b2, proxy));
}

/// The endurance timeline, second ordering: the primary reboots and reclaims
/// the ringing body while the proxy still reads it dead, THEN the callee
/// answers — the survivor's takeover copy answers alice — and only then does
/// the proxy route to the primary again. The primary's reclaimed copy must
/// take the survivor's answer, not re-answer the call at the ring deadline.
#[tokio::test(start_paused = true)]
async fn an_answer_served_by_the_takeover_copy_after_the_reclaim_draws_no_second_final() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("078-answered-on-backup-after-reclaim").await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    let (_, ringing) = replica(b2, &primary_ord, &call_ref).await;
    let fire_at = no_answer_deadline(&ringing);

    // ── the primary dies mid-ring, reboots and reclaims the ringing body while
    //    the proxy still reads it dead ─────────────────────────────────────────
    fh.mark(&primary_ord, None, "crash", "primary down while the callee rings");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;
    let new_addr = reboot_and_reclaim(&mut fh, b1, b2, &primary_ord, &proxy, false).await;
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed the ringing call");
    let hydrated_before = b2.metrics().repl_takeover_hydrated_total();

    // ── the callee answers: the proxy still routes to the survivor, whose
    //    takeover copy answers alice ────────────────────────────────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the callee's 2xx hydrated the call onto the survivor (takeover fired)",
    );

    // ── the proxy learns the primary's new address and reads it alive ────────
    announce_rebooted(&mut fh, &primary_ord, &proxy, new_addr);
    fh.mark(&primary_ord, None, "proxy alive", "endpoint + readiness caught up after the answer");
    fh.advance(Duration::from_secs(2)).await;
    fh.mark(
        &primary_ord,
        None,
        "before deadline",
        &format!(
            "primary serves={} survivor serves={}",
            b1.serves(&call_ref),
            b2.serves(&call_ref)
        ),
    );
    assert!(fh.now_ms() < fire_at, "the answer lands before the ring deadline");
    assert_answered_on(b1, &call_ref, "the primary folded the survivor's answer");

    let crossed = cross_deadline(&fh, &alice, &bob, fire_at).await;
    let passed = crossed.stray_final.is_none() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(
            &report_dir("078-answered-on-backup-after-reclaim"),
            "report",
            "078 — the answer served by the takeover copy after the reclaim",
            passed,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());
    assert_eq!(
        crossed.stray_final, None,
        "a second final on alice's already-answered INVITE server transaction (RFC 3261 §17.2.1)",
    );
    assert_eq!(crossed.cancels_to_bob, 0, "bob's answered INVITE takes no CANCEL");

    // ── the surviving call terminates normally: BYE end-to-end ───────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle_released(&fh, &alice, &bob, &[&*b1, &*b2], &call_ref).await;

    drop((w_b1, w_b2, proxy));
}

/// The endurance timeline, third ordering: the primary reclaimed the ringing
/// body, the survivor's takeover copy answers alice, and the proxy starts
/// routing to the primary before the survivor's answer version reaches it —
/// so alice's ACK lands on the primary's ringing copy first. The survivor's
/// answer must still be taken: no second final at the ring deadline.
#[tokio::test(start_paused = true)]
async fn an_answer_the_reclaimed_copy_saw_the_ack_of_first_draws_no_second_final() {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } =
        spawn_cluster("078-ack-lands-before-the-answer-folds").await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, bak_ord) = worker_ordinals(uas.request());
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    let (_, ringing) = replica(b2, &primary_ord, &call_ref).await;
    let fire_at = no_answer_deadline(&ringing);

    // ── the primary dies mid-ring, reboots and reclaims the ringing body while
    //    the proxy still reads it dead ─────────────────────────────────────────
    fh.mark(&primary_ord, None, "crash", "primary down while the callee rings");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_millis(300)).await;
    let new_addr = reboot_and_reclaim(&mut fh, b1, b2, &primary_ord, &proxy, false).await;
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed the ringing call");

    // ── the survivor's flushes toward the primary now take 3 s to land ──────
    fh.delay_streams_from(&bak_ord, &primary_ord, 3_000);

    // ── the callee answers: the proxy still routes to the survivor, whose
    //    takeover copy answers alice ────────────────────────────────────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    bob.receive("ACK").await;

    // ── the proxy reads the primary alive before the answer version lands:
    //    alice's ACK goes to the primary's ringing copy ─────────────────────────
    announce_rebooted(&mut fh, &primary_ord, &proxy, new_addr);
    fh.mark(&primary_ord, None, "proxy alive", "before the survivor's answer version lands");
    let mut dialog = call.ack().await;
    fh.advance(Duration::from_millis(500)).await;
    fh.advance(Duration::from_secs(4)).await; // the delayed answer version lands
    while bob.take_queued().await.is_some() {} // a re-relayed ACK, if any
    fh.mark(
        &primary_ord,
        None,
        "before deadline",
        &format!(
            "primary serves={} survivor serves={} folds refused={}",
            b1.serves(&call_ref),
            b2.serves(&call_ref),
            b1.metrics().repl_reverse_flush_refused_total()
        ),
    );
    assert!(fh.now_ms() < fire_at, "the answer lands before the ring deadline");
    assert_eq!(b1.metrics().repl_reverse_flush_refused_total(), 0, "the answer was not refused");
    assert_answered_on(
        b1,
        &call_ref,
        "the primary folded the survivor's answer over its own p bump",
    );

    let crossed = cross_deadline(&fh, &alice, &bob, fire_at).await;
    let passed = crossed.stray_final.is_none() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(
            &report_dir("078-ack-lands-before-the-answer-folds"),
            "report",
            "078 — alice's ACK reaches the reclaimed copy before the survivor's answer",
            passed,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());
    assert_eq!(
        crossed.stray_final, None,
        "a second final on alice's already-answered INVITE server transaction (RFC 3261 §17.2.1)",
    );
    assert_eq!(crossed.cancels_to_bob, 0, "bob's answered INVITE takes no CANCEL");

    // ── the surviving call terminates normally: BYE end-to-end ───────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle_released(&fh, &alice, &bob, &[&*b1, &*b2], &call_ref).await;

    drop((w_b1, w_b2, proxy));
}
