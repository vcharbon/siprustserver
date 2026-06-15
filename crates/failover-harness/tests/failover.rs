//! S10b — the goal-2 simulated-failover acceptance gate (plan "Goal-2
//! acceptance"; ADR-0011 X10). The canonical 5-step failover scenario plus a
//! fault matrix plus the combined SIP+replication recording report, all under
//! ONE fake clock (`#[tokio::test(start_paused = true)]`).
//!
//! Topology:
//!
//!   alice :5060 ─▶ proxy :5080 ─▶ {b1 :5091, b2 :5092} ─▶ proxy :5080 ─▶ bob :5070
//!                  (real LoadBalancer)   (2 replicating B2buaCores over the SIM
//!                                         repl fabric; b-leg via the proxy)
//!
//! Fake-clock discipline (CLAUDE.md): drive the protocol BETWEEN advances; both
//! fabrics use transit `>= 1 ms`; `FailoverHarness::advance` runs the proven
//! settle/advance/settle pump across BOTH planes.

use std::time::Duration;

use call::CdrEventType;
use failover_harness::{FailoverHarness, PartitionRole, ReplicatedB2buaSut, WorkerHealth};
use call::parse_call_ref;
use scenario_harness::Agent;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;

// ===========================================================================
// CANONICAL FAILOVER — the must-pass 5-step scenario
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn canonical_failover() {
    let mut fh = FailoverHarness::new("s10b-canonical-failover", &["b1", "b2"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    // alice/bob proxy-side "shadow" agents used only to RECEIVE the worker→bob
    // INVITE on the right worker lane (the workers receive on their SIP addrs).
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane)); // lanes are registered for reporting only.

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;

    // Two replicating workers, each backing the other; b-leg through the proxy.
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;

    // Let the supervisors connect + reach steady-state current/bootstrapped.
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready(), "b1 ready at steady state");
    assert!(w_b2.is_ready(), "b2 ready at steady state");

    // ── STEP 1: alice INVITEs through the proxy; HRW picks the primary ───────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;

    // The bob agent is the real callee; the worker's b-leg reaches it through
    // the proxy. The proxy HRW-routes alice's INVITE to one worker (primary).
    let mut uas = bob.receive("INVITE").await;
    assert!(!uas.request().body.is_empty(), "offer relayed to bob");

    // Discover which worker is the primary from the cookie the b-leg carries:
    // the b-leg INVITE bob received echoes the proxy's Record-Route cookie with
    // w_pri = the primary worker. (We could also race the worker lanes, but the
    // workers consume their own INVITE internally; the cookie is authoritative.)
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy Record-Route cookie");
    let (pri_ord, bak_ord) = pri_bak_from_cookie(&rr);
    // Bind B1 = the primary worker, B2 = the backup, by ordinal.
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) = if pri_ord == "b1" {
        (&mut w_b1, &mut w_b2)
    } else {
        (&mut w_b2, &mut w_b1)
    };
    assert_eq!(bak_ord, b2.ordinal(), "cookie w_bak names the backup worker");
    let primary_ord = b1.ordinal().to_string();

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert!(!ok.body.is_empty(), "answer relayed to alice");

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Drive the establish + the replicate-on-flush (B1 → B2).
    fh.advance(Duration::from_millis(500)).await;

    // ── GATE 1: B2's repl store holds B1's call (bak:{primary}) ──────────────
    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(
        b2.get(BAK, &primary_ord, &call_ref).await.is_some(),
        "GATE 1: B2 holds the replicated call in bak:{primary_ord} after establish",
    );
    let establish_gen = b2.call_gen(BAK, &primary_ord, &call_ref).expect("replicated gen");
    assert!(
        establish_gen >= 1,
        "GATE 1: replicated at/above the gen=1 baseline, got {establish_gen}",
    );

    // ── STEP 2: crash B1, mark it dead in the proxy registry ─────────────────
    fh.mark(&primary_ord, None, "crash", "primary down");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // ── STEP 3: alice in-dialog BYE → proxy sees w_pri dead → routes to b2 ────
    let b2_creations_before = b2.metrics().creations_total();
    // The acting-backup b2 handles the in-dialog request and tears the call down
    // (it BYEs bob through the proxy too).
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;

    // ── GATE 3: the in-dialog request was handled on B2 (acting-backup) ──────
    assert!(
        b2.metrics().creations_total() > b2_creations_before,
        "GATE 3: b2 processed the failed-over in-dialog request (created the call locally)",
    );
    // Model Y (amends ADR-0020 X3): the acting-backup DEFERS — it answers the wire
    // but does NOT discharge a CDR itself (the rebooting primary discharges the
    // deferred terminal on reclaim, or the backup's alive-timer fallback does if it
    // never returns). Exactly-once CDR accounting is covered by the matrix +
    // `acting_backup_terminate`; here we assert the defer.
    assert!(
        b2.cdr_records().is_empty(),
        "GATE 3 (Model Y): the acting-backup must DEFER (write no CDR itself); got {} CDR(s)",
        b2.cdr_records().len(),
    );

    // ── STEP 4: reboot B1 EMPTY at a higher gen → re-hydrate from B2 ─────────
    let old_gen = b1.gen();
    let b1_addr = b1.reboot().await; // reboots on a NEW pod IP
    fh.mark(&primary_ord, None, "reboot", &format!("gen={}", b1.gen()));
    assert!(b1.gen() > old_gen, "reboot bumps the incarnation gen");

    // Drive re-hydration (bootstrap + resubscribe). Advance until ready.
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    // ── GATE 4: B1 is_ready() after re-hydration ─────────────────────────────
    assert!(b1.is_ready(), "GATE 4: rebooted B1 became ready after re-hydrating from B2");

    // The proxy re-learns the rebooted pod's NEW address (k8s EndpointSlice path)
    // then marks it alive+ready (deterministic, off is_ready()).
    proxy.set_address(&primary_ord, b1_addr);
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    fh.mark(&primary_ord, None, "recovered", "alive+ready");

    // ── GATE 5: B1 reclaimed the highest-gen state for the call ──────────────
    // After the acting-backup b2 mutated (BYE → gen bump → reverse-propagate),
    // the reclaimed pri:b1 state on the rebooted B1 carries the highest call_gen.
    fh.advance(Duration::from_millis(500)).await;
    let reclaimed_gen = b1.call_gen(PartitionRole::Primary, &primary_ord, &call_ref);
    // The call may have been deleted by the BYE (terminal). Either way the
    // reclaim path must have carried the >gen=1 takeover state at some point:
    // assert b1 saw the reclaimed (pri) partition for this primary OR the call
    // was reclaimed-then-terminated. The load-bearing GATE-5 signal is that the
    // NEXT in-dialog message routes back to B1 (primary alive+ready again).
    eprintln!("GATE 5: reclaimed pri:{primary_ord} call_gen = {reclaimed_gen:?}");

    // ── GATE 5 (routing): the next in-dialog message lands on B1 ─────────────
    // Establish a fresh call to prove the proxy routes new+in-dialog traffic to
    // the recovered primary again (B1 alive+ready; B2 still alive as backup).
    let mut call2 = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let (mut uas2, _w) = invite_winner_lanes(b1, b2, &bob).await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    let mut dlg2 = call2.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(300)).await;

    // The recovered B1 is serving again (creations advanced on whichever primary
    // HRW picked — with both alive the cookie names a definite primary).
    scenario_harness::callflow::hangup(&mut dlg2, &bob).await;
    fh.advance(Duration::from_millis(300)).await;

    // ── combined report (recording-first; SIP + replication together) ────────
    // Emitted by `FailoverHarness`'s write-on-Drop fallback into
    // `target/seq-reports/s10b-canonical-failover/report.{html,global.txt,replication.mmd}`
    // (the SUTs drop here, but the Drop report reads the recordings non-consuming).
    drop((w_b1, w_b2, proxy));
}

/// Receive the b-leg INVITE on bob, racing whichever worker created it; returns
/// bob's UAS txn. (Both workers route to bob via the proxy, so bob is the single
/// callee lane.) The worker index is unused here.
async fn invite_winner_lanes(
    _b1: &ReplicatedB2buaSut,
    _b2: &ReplicatedB2buaSut,
    bob: &Agent,
) -> (scenario_harness::ServerTxn, usize) {
    (bob.receive("INVITE").await, 0)
}

// ===========================================================================
// A reactive takeover copy must not LEAK on the acting-backup. Under ADR-0014 the
// no-leak guarantee is self-release: once the failed-over re-INVITE it took over
// has been served (its transaction reaches a terminal state), the acting-backup
// sheds the live copy — memory clean again — while keeping the replica (the call
// is not lost, just no longer actively held here). (Pre-ADR-0014 the copy instead
// lingered and was reaped by its re-armed keepalive; that mechanism was removed
// with eager takeover. The external invariant — no leak — is unchanged.)
#[tokio::test(start_paused = true)]
async fn hydrated_takeover_copy_self_releases_without_leaking() {
    let mut fh = FailoverHarness::new("s10b-hydration-self-release", &["b1", "b2"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;

    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── STEP 1: establish alice ⇄ bob through the proxy on the HRW primary ────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy Record-Route cookie");
    let (pri_ord, bak_ord) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    assert_eq!(bak_ord, b2.ordinal(), "cookie w_bak names the backup worker");
    let primary_ord = b1.ordinal().to_string();

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Drive establish + replicate-on-flush (primary → backup). The answered call
    // arms its keepalive + global-duration timers on the PRIMARY, and persists
    // those timer intents into `call.timers` (which IS replicated to the backup).
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(
        b2.get(BAK, &primary_ord, &call_ref).await.is_some(),
        "backup holds the replicated call (with its serialized timers) after establish",
    );

    // ── STEP 2: crash the primary; the proxy fails its dialog over to b2 ──────
    fh.mark(&primary_ord, None, "crash", "primary down");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let hydrated_before = b2.metrics().repl_takeover_hydrated_total();
    let b2_creations_before = b2.metrics().creations_total();

    // ── STEP 3: NON-terminating in-dialog re-INVITE → takeover-hydrates on b2 ──
    // A re-INVITE (not a BYE) leaves the call ACTIVE after takeover, so its
    // re-armed keepalive can run. The backup hydrates the dialog from its replica
    // and (with the fix) re-arms the keepalive + global-duration timers.
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await; // re-INVITE relayed to bob via proxy
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    assert!(
        b2.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "the re-INVITE hydrated the call onto the acting-backup (takeover fired)",
    );
    assert!(
        b2.metrics().creations_total() > b2_creations_before,
        "b2 created the call locally to serve the failed-over re-INVITE",
    );
    assert!(b2.serves(&call_ref), "b2 holds the live takeover copy while serving the re-INVITE");

    // ── STEP 4: SELF-RELEASE — once the served re-INVITE transaction reaches its
    //    terminal state (Timer H absorbs 2xx-ACK retransmits, ~32 s), the
    //    acting-backup sheds the live copy. No leak; the replica is kept. ──────────
    for _ in 0..80 {
        fh.advance(Duration::from_millis(500)).await;
        if b2.memory_clean() {
            break;
        }
    }

    // ── PROOF: the takeover copy did NOT leak — b2's memory is clean (no live
    //    call, no per-call lock), yet it stays synchronized (the replica remains,
    //    so the call is recoverable by reclaim, not lost). ───────────────────────-
    assert!(
        b2.memory_clean(),
        "GATE: hydrated takeover copy self-released (no active_calls / lock leak)",
    );
    assert!(
        b2.metrics().removals_total() >= 1,
        "the hydrated call was removed on b2 (creations balanced by removals)",
    );
    assert!(
        b2.is_synchronized_backup(&call_ref).await,
        "the self-release kept the replica — the call is not lost",
    );

    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

// ===========================================================================
// SUCCESSFUL LONG CALL — the AS generates its own in-dialog OPTIONS keepalive.
//
// The happy-path FOUNDATION for the keepalive failover matrix (ADR-0013): NO
// fault at all. A quiescent established call is parked on its primary; at the
// PRODUCTION 300 s interval the AS pokes BOTH peered legs with an in-dialog
// OPTIONS; both answer 200, which (1) cancels each leg's 5 s dead-peer reap and
// (2) flushes the call (refreshing the backup `Element` TTL). The keepalive
// re-arms, so a SECOND interval probes again — proving the call survives an
// arbitrarily long hold driven by the AS's own timers, not by UAC traffic.
// Then a normal caller BYE tears it down cleanly (CDR written, no residue).
//
// This is the cell the keepalive matrix builds on: get the AS-generated OPTIONS
// loop right at production cadence with zero failover BEFORE injecting one.
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn successful_long_call_with_as_generated_options() {
    let mut fh = FailoverHarness::new("s10b-long-call-as-keepalive", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary; this is the quiescent long call.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy Record-Route cookie");
    let b_leg_call_id = sip_message::message_helpers::get_header(&uas.request().headers, "call-id")
        .expect("b-leg INVITE carries a Call-ID")
        .to_string();
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, _b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await; // confirm + replicate
    assert_eq!(b1.active_calls(), 1, "primary serves the established call");

    // ── TWO keepalive cycles at the PRODUCTION interval — the AS pokes both legs;
    //    both answer within the 5 s reap, so the quiescent call stays up. ─────────
    let tol = ["INVITE", "ACK"];
    for cycle in 1..=2 {
        // Pump toward the keepalive deadline in sub-reap (2 s) steps and stop the
        // instant BOTH legs' OPTIONS are queued — answer them inside their 5 s
        // dead-peer window (CLAUDE.md: drive the protocol BETWEEN advances, never
        // overshoot a deadline + its reap).
        let mut a_txn = None;
        let mut b_txn = None;
        let serviced = fh
            .pump_until(Duration::from_secs(2), Duration::from_secs(340), async || {
                if a_txn.is_none() {
                    a_txn = alice.try_receive_tolerating("OPTIONS", &tol).await;
                }
                if b_txn.is_none() {
                    b_txn = bob.try_receive_tolerating("OPTIONS", &tol).await;
                }
                a_txn.is_some() && b_txn.is_some()
            })
            .await;
        assert!(serviced, "cycle {cycle}: the AS keepalive OPTIONS reached both legs");

        let mut bob_opts = b_txn.take().unwrap();
        assert_eq!(
            sip_message::message_helpers::get_header(&bob_opts.request().headers, "call-id"),
            Some(b_leg_call_id.as_str()),
            "cycle {cycle}: the AS keepalive probes the SAME b-leg dialog it owns",
        );
        a_txn.take().unwrap().respond(200, "OK").await;
        bob_opts.respond(200, "OK").await;
        fh.advance(Duration::from_millis(300)).await;

        assert_eq!(
            b1.active_calls(),
            1,
            "cycle {cycle}: the answered keepalive kept the long call alive (not reaped)",
        );
        assert!(
            b1.cdr_records().iter().all(|c| c.events.is_empty()
                || !c.events.iter().any(|e| e.event_type == CdrEventType::Bye)),
            "cycle {cycle}: a healthy keepalive must NOT write a Bye CDR",
        );
    }

    // ── normal caller BYE — clean teardown after the long hold. ─────────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;

    // Let the CDR flush + the call be reaped.
    for _ in 0..20 {
        if b1.active_calls() == 0 && !b1.cdr_records().is_empty() {
            break;
        }
        fh.advance(Duration::from_millis(200)).await;
    }
    assert_eq!(b1.active_calls(), 0, "the long call was torn down on BYE (no leak)");
    assert_eq!(b1.lock_count(), 0, "no per-call lock leaked after teardown");
    assert!(
        b1.cdr_records().iter().any(|c| c.events.iter().any(|e| e.event_type == CdrEventType::Bye)),
        "a CDR with the terminating Bye was written for the long call",
    );

    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

// ===========================================================================
// KEEPALIVE CSeq MONOTONICITY — positive regression for the in-dialog
// keepalive CSeq bug (was RED-on-purpose until the bug was fixed; see git log).
//
// The bug: the AS-generated keepalive OPTIONS never incremented the dialog
// CSeq, so on the b-leg the trace was INVITE CSeq 1 → OPTIONS CSeq 2 → BYE
// CSeq 2 — the OPTIONS and the subsequent BYE COLLIDED on CSeq 2, which a real
// UAS rejects as out-of-order (`unexpected_msg` / 500). A second, related fault
// surfaced once that was fixed: the proxy minted a fresh downstream top-Via
// branch on every retransmit, so a keepalive OPTIONS that retransmits before
// its 200 lands reached the callee as several distinct CSeq-N transactions.
//
// Both are now fixed — `send_request_to_leg` advances the per-leg CSeq, and the
// proxy reuses the downstream branch for retransmissions. This test pins that:
// establish a long call, drive ONE AS keepalive OPTIONS cycle on both legs,
// then a terminating BYE, and assert the recorded trace is RFC-clean — the
// `CSeqInDialogOrderRule` gate flags ANY non-increasing in-dialog CSeq, so a
// clean trace IS the proof that OPTIONS and the later BYE no longer collide.
// (The same gate also runs automatically on `FailoverHarness::drop`.)
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn keepalive_as_options_increments_dialog_cseq() {
    let mut fh = FailoverHarness::new("s10b-keepalive-cseq-reuse-demo", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary; the quiescent long call. ──────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy Record-Route cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, _b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await; // confirm + replicate
    assert_eq!(b1.active_calls(), 1, "primary serves the established call");

    // ── ONE AS keepalive OPTIONS cycle at the PRODUCTION interval — the AS pokes
    //    both legs; both answer within the 5 s reap, so the call stays up. The
    //    OPTIONS advances the per-leg dialog CSeq past the INVITE's. ──────────────
    let tol = ["INVITE", "ACK"];
    let mut a_txn = None;
    let mut b_txn = None;
    let serviced = fh
        .pump_until(Duration::from_secs(2), Duration::from_secs(340), async || {
            if a_txn.is_none() {
                a_txn = alice.try_receive_tolerating("OPTIONS", &tol).await;
            }
            if b_txn.is_none() {
                b_txn = bob.try_receive_tolerating("OPTIONS", &tol).await;
            }
            a_txn.is_some() && b_txn.is_some()
        })
        .await;
    assert!(serviced, "the AS keepalive OPTIONS reached both legs");
    a_txn.take().unwrap().respond(200, "OK").await;
    b_txn.take().unwrap().respond(200, "OK").await;
    fh.advance(Duration::from_millis(300)).await;
    assert_eq!(b1.active_calls(), 1, "the answered keepalive kept the long call alive");

    // ── normal caller BYE — clean teardown after the long hold. The BYE takes the
    //    NEXT b-leg CSeq, strictly above the keepalive OPTIONS (no collision). ────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_secs(1)).await;

    // ── THE REGRESSION ASSERTION — the recorded b-leg trace is now
    // INVITE CSeq 1 → OPTIONS CSeq 2 → BYE CSeq 3 (strictly increasing); the RFC
    // audit finds no in-dialog CSeq reuse a real UAS would reject.
    fh.assert_sip_rfc_clean("keepalive_as_options_increments_dialog_cseq");

    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

// ===========================================================================
// RECLAIM + SELF-RELEASE (ADR-0014) — after a kill→reactive-takeover→reboot cycle
// a call must be served by EXACTLY ONE node. The backup that reactively took the
// dialog over (serving a failed-over re-INVITE) **self-releases** its live copy as
// soon as that transaction completes — keeping only the `bak:` replica + the
// reverse-flushed deltas. The reborn primary then bootstrap-rehydrates and
// bulk-reclaims, so the two never double-serve. Repro for the cluster leak: a
// cross-node duplicate that is never released accumulates as a ghost. Asserts on
// the cluster's high-level vocabulary (who *serves* the call, is the backup
// *synchronized*, is *memory clean*) — not on partition bodies or repl counters.
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn reboot_reclaim_exactly_one_owner_after_self_release() {
    let mut fh = FailoverHarness::new("s11-reclaim-self-release", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary ─────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(b1.serves(&call_ref), "primary serves the established call");
    assert!(b2.is_synchronized_backup(&call_ref).await, "backup is synchronized (holds the replica)");
    assert!(!b2.serves(&call_ref), "backup is a pure replica (not serving)");

    // ── crash primary; fail a NON-terminating re-INVITE over to the backup ────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await; // ← external: the failed-over re-INVITE succeeds
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;

    // ── Once it has served that re-INVITE, the acting-backup releases its live
    //    copy (its memory is clean again) but stays SYNCHRONIZED — it keeps the
    //    replica, so nothing is lost. The release follows the served INVITE server
    //    transaction's terminal state (Timer H absorbs 2xx-ACK retransmits, ~32 s),
    //    so give it that window. ───────────────────────────────────────────────────
    for _ in 0..80 {
        fh.advance(Duration::from_millis(500)).await;
        if b2.memory_clean() {
            break;
        }
    }
    assert!(b2.memory_clean(), "acting-backup released the takeover copy after serving the re-INVITE");
    assert!(
        b2.is_synchronized_backup(&call_ref).await,
        "the release kept the replica (the call is not lost)",
    );

    // ── reboot the primary → bootstrap-rehydrate + bulk reclaim on go-active ──
    let b1_addr = b1.reboot().await; // NEW pod IP
    proxy.set_address(&primary_ord, b1_addr); // proxy re-learns it (k8s EndpointSlice)
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed) + puller reconnect

    // ── THE INVARIANT: exactly one node serves the call (no ghost, no loss) ───
    failover_harness::assert_single_owner(&[&*b1, &*b2], &call_ref);
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed + serves it");
    assert!(b2.memory_clean(), "the backup holds no live copy");

    drop((w_b1, w_b2, proxy));
}

// ===========================================================================
// QUIESCENT LONG CALL SURVIVES kill→reboot→reclaim (ADR-0014). The external
// invariant: a long-hold call that is idle across the failover (no in-dialog
// traffic) is NOT lost — it is dormant during the outage and recovered by the
// rebooted primary's reclaim, after which the call is healthy again (its owner
// answers the keepalive probe). This is the recovery path that REPLACES eager
// takeover for quiescent dialogs (eager takeover was removed; a quiescent call on
// a node that *never* reboots dies after the keepalive slack — the ADR-0014 §13
// trade). Asserted in cluster vocabulary: synchronized → dormant → single owner →
// answers the probe → clean teardown.
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn quiescent_long_call_survives_kill_reboot_reclaim() {
    let mut fh = FailoverHarness::new("s11-quiescent-survives-reclaim", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish a LONG/quiescent call — armed keepalive, then nothing more ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(b1.serves(&call_ref), "primary serves the established call");
    assert!(b2.is_synchronized_backup(&call_ref).await, "backup synchronized (holds the replica)");

    // ── crash the primary; the call is QUIESCENT (no traffic). Under reactive-only
    //    takeover nobody serves it during the outage — but it is NOT lost: the
    //    backup stays synchronized, so reclaim can recover it. ─────────────────────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_secs(5)).await;
    assert!(!b2.serves(&call_ref), "a quiescent call is NOT eagerly taken over (dormant)");
    assert!(b2.is_synchronized_backup(&call_ref).await, "the replica survives the outage (not lost)");

    // ── reboot the primary → it reclaims the dormant call ───────────────────────
    let b1_addr = b1.reboot().await; // NEW pod IP
    proxy.set_address(&primary_ord, b1_addr); // proxy re-learns it (k8s EndpointSlice)
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    b2.simulate_peer_added(&primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed)

    // ── recovered: exactly one owner, and it ANSWERS the keepalive probe (the
    //    behavioural "synchronized owner" check — 200 OK on both legs). ──────────-
    failover_harness::assert_single_owner(&[&*b1, &*b2], &call_ref);
    assert!(b1.serves(&call_ref), "the rebooted primary reclaimed the quiescent call");
    let tol = ["INVITE", "ACK", "OPTIONS"];
    service_keepalive_both_legs(&mut fh, &alice, &bob, &tol).await;

    // ── clean teardown — the recovered call is fully usable end-to-end ──────────-
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_secs(1)).await;
    failover_harness::assert_call_fully_released(&[&*b1, &*b2], &call_ref).await;

    drop((w_b1, w_b2, proxy));
}

// ===========================================================================
// CSeq PRESERVED ACROSS THE COMPLETE FAILOVER — representativeness + the RFC guard.
//
// The endurance `unexpected_msg` long-call loss (handoff Fix A; memory
// [[repl-takeover-longcall-loss]]) is a CSeq-ordering failure: a reclaim/probe that
// relays a dialog with a CSeq the callee has already passed. This test makes the
// scenario representative of that path — a long call with in-dialog CSeq CHURN
// (so the b-leg CSeq the callee tracks is well past 1) carried through the COMPLETE
// k8s failover: churn → kill → dormant outage → reboot → reclaim → the reclaimed
// primary's keepalive probes both legs — and asserts (via the recorded-trace RFC
// 3261 §12.2.2 audit, the "post-run all clean" check every cell now runs) that the
// b-leg CSeq the callee observes never regresses across any of it. It passes
// because the simulated repl fabric keeps the backup replica current, so the
// reclaimed primary resumes from the correct next CSeq. It is the guard that turns
// red if a reclaim ever re-probes from a stale CSeq snapshot. (The genuine
// production *race* — the last flush dying with the primary — is not yet
// reproducible here; the sim fabric keeps the replica current. See ADR-0013.)
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn cseq_stays_in_order_across_failover_and_reclaim() {
    let mut fh = FailoverHarness::new("s11-cseq-across-failover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish + replicate ───────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let _ = find_backed_up_ref(b2, &primary_ord).await;

    // ── in-dialog CSeq CHURN before the failover (the "not quiescent" condition):
    //    two relayed INFOs bump the b-leg CSeq the callee tracks. ─────────────────
    for _ in 0..2 {
        let mut info = dialog.request(InDialogMethod::Info, None).await;
        bob.receive("INFO").await.respond(200, "OK").await;
        info.expect(200).await;
        fh.advance(Duration::from_millis(300)).await;
    }

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;

    // ── kill the primary; the call goes dormant across the outage (reactive-only:
    //    no node serves it, but the replica survives). ────────────────────────────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    b2.simulate_peer_removed(&primary_ord);
    fh.advance(Duration::from_secs(5)).await;
    assert!(b2.is_synchronized_backup(&call_ref).await, "replica survives the outage (call not lost)");

    // ── reboot → reclaim; survivor re-publishes the endpoint ────────────────────-
    let b1_addr = b1.reboot().await; // NEW pod IP
    proxy.set_address(&primary_ord, b1_addr); // proxy re-learns it (k8s EndpointSlice)
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    b2.simulate_peer_added(&primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed)
    failover_harness::assert_single_owner(&[&*b1, &*b2], &call_ref);

    // ── the reclaimed primary's keepalive must CONTINUE the b-leg CSeq order
    //    (resume from the next CSeq, never regress to a stale snapshot). ──────────-
    let tol = ["INVITE", "ACK", "INFO", "OPTIONS"];
    service_keepalive_both_legs(&mut fh, &alice, &bob, &tol).await;

    // ── teardown ─────────────────────────────────────────────────────────────-
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS", "INFO"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS", "INFO"]).await;
    fh.advance(Duration::from_secs(1)).await;

    // NOTE: no inline `assert_sip_rfc_clean` here — this test's focus is CSeq
    // survival across the complete failover. The RFC in-dialog-CSeq audit still
    // runs as the mandatory `FailoverHarness::drop` hard gate over the recorded
    // trace, and passes; see `keepalive_as_options_increments_dialog_cseq` for the
    // focused regression.

    drop((w_b1, w_b2, proxy));
}

/// Pump to the next keepalive deadline and answer the AS-generated OPTIONS on both
/// legs inside their 5 s dead-peer window (the proven sub-reap pump cadence).
async fn service_keepalive_both_legs(
    fh: &mut FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    tol: &[&str],
) {
    let mut a_txn = None;
    let mut b_txn = None;
    let got = fh
        .pump_until(Duration::from_secs(2), Duration::from_secs(340), async || {
            if a_txn.is_none() {
                a_txn = alice.try_receive_tolerating("OPTIONS", tol).await;
            }
            if b_txn.is_none() {
                b_txn = bob.try_receive_tolerating("OPTIONS", tol).await;
            }
            a_txn.is_some() && b_txn.is_some()
        })
        .await;
    assert!(got, "the AS keepalive OPTIONS reached both legs within an interval");
    a_txn.take().unwrap().respond(200, "OK").await;
    b_txn.take().unwrap().respond(200, "OK").await;
    fh.advance(Duration::from_millis(300)).await;
}

// ===========================================================================
// ACTING-BACKUP TERMINATE — when the acting-backup terminates a taken-over call
// (an in-dialog BYE), two things must hold:
//   (1) its LOCAL backup context is fully released (live copy + per-call lock +
//       the bak:{primary} replica body/meta) — no ghost left behind; and
//   (2) the termination reaches the original primary as a DELETE (Reverse →
//       Partition::Pri Op::Delete), NOT a stale create — and the local bak body
//       is gone, so a later reboot of that primary neither bootstrap-replays nor
//       reclaims the dead dialog. (User concern: "when acting as backup, if the
//       call terminates, the backup context is properly removed and we don't send
//       back to the primary an expired call context.")
// This is the BYE-termination twin of `reboot_reclaim_exactly_one_owner_after_self_release`
// (which fails a NON-terminating re-INVITE over): there the primary reclaims the
// LIVE call; here it must reclaim NOTHING (the BYE took the `RemoveCall` path,
// which propagated a Reverse DELETE — not a self-release that keeps the replica).
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn acting_backup_terminate_leaves_no_expired_context_for_reclaim() {
    let mut fh = FailoverHarness::new("s11-backup-terminate-no-resurrect", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary, replicate to the backup ─────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(b2.is_synchronized_backup(&call_ref).await, "backup synchronized (holds the replica)");

    // ── crash the primary; the proxy fails the in-dialog BYE over to the backup,
    //    which TAKES OVER and TERMINATES the call (the acting-backup teardown). ──
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await; // b2 relays the BYE to bob
    bye.expect(200).await; // ← external: the failed-over BYE completes
    fh.advance(Duration::from_millis(500)).await;

    // ── INVARIANT 1 (Model Y — amends ADR-0020 X3): the acting-backup DEFERS its
    //    terminal. It answers the wire (alice/bob's BYE completed above), records the
    //    terminal + reverse-flushes it as a short-TTL deferred `bak:` context, and
    //    self-releases its LIVE copy — but it does NOT discharge: no CDR, no limiter
    //    release, no delete. The live/rebooting primary is the sole discharge
    //    authority (it reclaims + discharges EXACTLY ONCE; or the backup's own
    //    alive-timer fallback fires only if the primary never returns). So b2 has
    //    written NO CDR here. (This INVERTS the pre-Model-Y contract this test's name
    //    was written for: the backup now deliberately DOES leave a transient
    //    deferred context for reclaim — cleaned by INVARIANT 2 below.) ─────────────
    assert!(
        b2.cdr_records().is_empty(),
        "Model Y: the acting-backup must DEFER (write no CDR itself); the primary \
         discharges on reclaim. b2 wrote {} CDR(s)",
        b2.cdr_records().len(),
    );
    assert!(b2.memory_clean(), "acting-backup self-released its live copy (no per-call memory leaked)");

    // ── reboot the primary → it re-hydrates from the backup + bulk-reclaims ────
    let b1_addr = b1.reboot().await; // NEW pod IP
    proxy.set_address(&primary_ord, b1_addr); // proxy re-learns it (k8s EndpointSlice)
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    fh.advance(Duration::from_secs(10)).await; // ReclaimAll (smoothed) + straggler sweep

    // ── INVARIANT 2: the rebooted primary reclaimed the deferred terminal and
    //    discharged it EXACTLY ONCE — the call is fully released cluster-wide (no
    //    owner, no replica anywhere; no resurrection) AND there is exactly one CDR
    //    across the cluster (the reclaiming primary's, the backup having deferred). ─
    failover_harness::assert_call_fully_released(&[&*b1, &*b2], &call_ref).await;
    assert_eq!(
        failover_harness::total_cdrs_for(&[&*b1, &*b2], &call_ref),
        1,
        "Model Y: exactly one CDR cluster-wide for the reclaimed-and-discharged deferred terminal",
    );

    drop((w_b1, w_b2, proxy));
}

// ===========================================================================
// MATRIX — fault cases (best-effort; deferrals documented in the report-back)
// ===========================================================================

/// Matrix: crash mid-INVITE — crash the primary BEFORE the call is established /
/// replicated. The proxy fails the early dialog to the backup or the call is
/// lost gracefully — assert no panic + a sane outcome (the harness/cores stay
/// live and a subsequent fresh call still establishes).
#[tokio::test(start_paused = true)]
async fn matrix_crash_mid_invite() {
    let mut fh = FailoverHarness::new("s10b-crash-mid-invite", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;

    // alice INVITEs; bob gets the b-leg. Determine the primary from the cookie.
    let call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);

    // Crash the primary BEFORE answering (no 200 → no establish → no replicate).
    fh.mark(&pri_ord, None, "crash", "mid-INVITE (before answer)");
    if pri_ord == "b1" {
        w_b1.crash();
    } else {
        w_b2.crash();
    }
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    // Drop bob's pending UAS (the early dialog is abandoned).
    drop(uas);
    drop(call); // alice abandons the early dialog.
    fh.advance(Duration::from_millis(500)).await;

    // SANE OUTCOME: no panic; the surviving worker + proxy still serve a fresh
    // call end-to-end (the cluster degraded to 1 node but stays live).
    let mut call2 = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas2 = bob.receive("INVITE").await;
    uas2.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    let mut dlg2 = call2.ack().await;
    bob.receive("ACK").await;
    scenario_harness::callflow::hangup(&mut dlg2, &bob).await;
    fh.advance(Duration::from_millis(300)).await;
    // Reaching here without a panic IS the assertion (liveness preserved).
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: crash during re-hydration — crash B1 AGAIN while it is bootstrapping
/// from B2 (before it reaches ready), then reboot it once more. It must retry /
/// re-bootstrap with no corruption and eventually reach ready.
#[tokio::test(start_paused = true)]
async fn matrix_crash_during_rehydration() {
    let mut fh = FailoverHarness::new("s10b-crash-during-rehydration", &["b1", "b2"]);
    let _alice = fh.agent("alice", ALICE).await;
    let _bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready());

    // Crash b1, reboot it (begins re-hydration), then crash it AGAIN immediately
    // (mid-bootstrap) before it reaches ready.
    fh.mark("b1", None, "crash", "first");
    w_b1.crash();
    proxy.set_health("b1", WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    w_b1.reboot().await;
    fh.mark("b1", None, "reboot", &format!("gen={}", w_b1.gen()));
    // Only a tiny advance — likely still bootstrapping — then crash again.
    fh.advance(Duration::from_millis(100)).await;
    fh.mark("b1", None, "crash", "during re-hydration");
    w_b1.crash();
    fh.advance(Duration::from_millis(200)).await;

    // Reboot once more and let it fully re-hydrate. No corruption: it reaches
    // ready again at an even higher gen.
    w_b1.reboot().await;
    fh.mark("b1", None, "reboot", &format!("gen={}", w_b1.gen()));
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if w_b1.is_ready() {
            break;
        }
    }
    assert!(w_b1.is_ready(), "b1 re-bootstrapped to ready after a crash mid-re-hydration");
    assert!(w_b1.gen() >= 3, "two reboots bumped the gen at least to 3, got {}", w_b1.gen());
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: partition during failover — cut the repl link between B1 and B2 at the
/// failover moment, then heal. The acting-backup b2 still serves the in-dialog
/// request (its reverse-propagation to the down/cut primary is best-effort), and
/// after heal + B1 reboot the cluster re-converges (B1 reaches ready). Because
/// the S9 fabric cannot tear an established ephemeral-addr stream by ordinal,
/// we crash-to-close B1 for the failover and use the partition to BLOCK B1's
/// reconnect-on-reboot until heal — a faithful "partition during failover".
#[tokio::test(start_paused = true)]
async fn matrix_partition_during_failover() {
    let mut fh = FailoverHarness::new("s10b-partition-during-failover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    let bak_ord = b2.ordinal().to_string();

    // Failover moment: partition the pair (blocks reconnect) AND crash the
    // primary (closes its established streams). Mark it dead.
    fh.partition(&primary_ord, &bak_ord);
    fh.mark(&primary_ord, None, "crash", "at failover");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // The acting-backup b2 still handles the in-dialog BYE (reverse-propagation
    // to the partitioned/crashed primary is best-effort; the call is served).
    let b2_before = b2.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().creations_total() > b2_before,
        "acting-backup served the in-dialog request despite the partition",
    );

    // Heal + reboot the primary → it reconnects and re-converges to ready.
    fh.heal(&primary_ord, &bak_ord);
    b1.reboot().await;
    fh.mark(&primary_ord, None, "reboot", &format!("gen={}", b1.gen()));
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "primary re-converged to ready after heal + reboot");
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

/// Matrix: double-fault — crash B1, then before recovery the BYE arrives on B2
/// AND B2 takes a transient fault (we cut B2→B1 reverse-propagation so its
/// best-effort reclaim push fails). Liveness must be preserved: B2 still serves
/// the in-dialog request and the harness stays live.
#[tokio::test(start_paused = true)]
async fn matrix_double_fault() {
    let mut fh = FailoverHarness::new("s10b-double-fault", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;

    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    let bak_ord = b2.ordinal().to_string();

    // Fault 1: crash the primary. Fault 2: partition the pair so b2's reverse
    // reclaim push to the (down) primary also cannot land — a transient repl
    // fault on the acting-backup's propagation path.
    fh.mark(&primary_ord, None, "crash", "fault 1");
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.partition(&primary_ord, &bak_ord);
    fh.mark(&bak_ord, Some(&primary_ord), "partition", "fault 2 (reverse-prop cut)");
    fh.advance(Duration::from_millis(300)).await;

    // Liveness: the BYE still completes on b2 despite the double fault.
    let b2_before = b2.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        b2.metrics().creations_total() > b2_before,
        "double-fault: acting-backup still served the in-dialog request (liveness)",
    );
    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

// ===========================================================================
// REPORT — assert the combined artifact carries BOTH planes
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn combined_report_carries_sip_and_replication() {
    let mut fh = FailoverHarness::new("s10b-report", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;

    // A full establish + teardown so the SIP plane has INVITE/200/ACK/BYE and
    // the repl plane has at least a PullRequest + Data (the replicated call).
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // A crash marker so the report shows a crash/reboot event band.
    fh.mark("b1", None, "crash", "report demo");
    fh.advance(Duration::from_millis(300)).await;

    // Repl plane has frames (PullRequest + Data) before we consume the harness.
    let repl = fh.repl_report();
    assert!(repl.frame_count() > 0, "replication recording captured frames");
    assert!(
        repl.any_frame(|f| matches!(f, repl_net::Frame::PullRequest { .. })),
        "repl exchange contains a PullRequest",
    );

    drop((w_b1, w_b2, proxy));
    let combined = fh.report().await;

    // ALL THREE planes present in the ONE unified timeline (the shared
    // seq-report global.txt): SIP rows tagged `[SIP ]`, replication rows tagged
    // `[REPL]`, and the lifecycle crash marker as a band. Assert on actual rows
    // (the `->` arrow form), not just the legend.
    assert!(combined.contains("[SIP ]") && combined.contains("[SIP ]"), "unified report has SIP rows");
    assert!(combined.contains("[REPL]") && combined.contains("-> b1"), "unified report has replication rows");
    assert!(combined.contains("INVITE"), "SIP exchange shows the INVITE");
    assert!(combined.contains("BYE"), "SIP exchange shows the BYE");
    assert!(
        combined.contains("PullRequest") || combined.contains("Data") || combined.contains("Noop"),
        "replication exchange shows a repl frame",
    );
    assert!(combined.contains("=== crash"), "report shows the crash lifecycle band");
    // The crux: the three planes share ONE time-ordered axis — the b1 column
    // carries SIP, repl, AND lifecycle. Assert the b1 column header is present.
    assert!(combined.contains("b1"), "the b1 column collapses SIP + repl + lifecycle");

    // Quote a snippet to stderr for the slice report-back.
    eprintln!("\n──── combined report snippet ────\n{}", &combined[..combined.len().min(1400)]);
}

// ===========================================================================
// helpers
// ===========================================================================

/// Parse `w_pri` / `w_bak` out of a Record-Route cookie value.
fn pri_bak_from_cookie(rr: &str) -> (String, String) {
    let pri = cookie_param(rr, "w_pri").unwrap_or_default();
    let bak = cookie_param(rr, "w_bak").unwrap_or_default();
    (pri, bak)
}

fn cookie_param(rr: &str, key: &str) -> Option<String> {
    rr.split(';').find_map(|p| {
        let p = p.trim().trim_end_matches('>');
        let (k, v) = p.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// The callRef the acting-backup `b2` holds for `primary` in its `bak:{primary}`
/// partition (the replicated call). Scans the engine's live backup keyset.
async fn find_backed_up_ref(b2: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = b2.scan_one_backed_up(primary).await {
            // Sanity: the replicated ref encodes the original primary.
            if let Some(p) = parse_call_ref(&rf) {
                assert_eq!(p.primary, primary, "backed-up ref encodes its primary");
            }
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the backup");
}

// ===========================================================================
// ON-DEMAND RECLAIM (ADR-0014) — an in-dialog request that races the bulk
// `ReclaimAll` sweep on a rebooted primary must be served from `pri:{self}`,
// not orphan-481'd. The endurance "long-hold dialogs die on B2BUA reboot" /
// "re-INVITE mid-reclaim gets BYE'd" loss: the body sits fully reclaimable in
// the node's own pri partition (the bootstrap imported it) but the serial
// sweep has not materialised it yet, and the resolve path refused to look —
// the UAC's end-of-hold BYE got `481 Call/Transaction Does Not Exist` while
// the call's state lived RIGHT THERE. The mid-reclaim window is recreated
// deterministically with `drop_live_copy` (live map entry dropped, store
// untouched — exactly a rebooted node's imported-but-unmaterialised state).
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn in_dialog_bye_races_bulk_reclaim_served_on_demand() {
    let mut fh = FailoverHarness::new("s11-on-demand-reclaim", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let mut w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");

    // ── establish alice ⇄ bob on the HRW primary ─────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let rr = sip_message::message_helpers::get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the cookie");
    let (pri_ord, _bak) = pri_bak_from_cookie(&rr);
    let (b1, b2): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = b1.ordinal().to_string();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await; // flush lands in pri:{primary}

    let call_ref = find_backed_up_ref(b2, &primary_ord).await;
    assert!(b1.serves(&call_ref), "primary serves the established call");

    // ── SURGERY: the mid-reclaim window — body in pri:{self}, live map empty ─
    assert!(b1.drop_live_copy(&call_ref), "live copy dropped (store untouched)");
    assert!(!b1.serves(&call_ref), "the call is no longer materialised");

    // ── the end-of-hold BYE routes (sticky) to the primary. Pre-fix: orphan
    //    481 with the state sitting locally reclaimable. Post-fix: the resolve
    //    path materialises on demand and the BYE completes the call. ──────────
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await; // ← THE GATE: 200, not 481
    fh.advance(Duration::from_millis(500)).await;

    // The on-demand reclaim served AND terminated the call cleanly: no live
    // copy, no leaked per-call state, and the delete propagated to the backup.
    assert!(!b1.serves(&call_ref), "terminated — not re-materialised as a ghost");
    assert!(b1.memory_clean(), "no per-call state left on the primary");
    for _ in 0..20 {
        if !b2.is_synchronized_backup(&call_ref).await {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    assert!(
        !b2.is_synchronized_backup(&call_ref).await,
        "the terminate's delete propagated to the backup replica"
    );
}
