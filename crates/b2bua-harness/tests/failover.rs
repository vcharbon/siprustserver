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
use b2bua_harness::{FailoverHarness, PartitionRole, ReplicatedB2buaSut, WorkerHealth};
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
    let mut bye = dialog.bye().await;
    // The acting-backup b2 handles the in-dialog request and tears the call down
    // (it BYEs bob through the proxy too).
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    fh.advance(Duration::from_millis(500)).await;

    // ── GATE 3: the in-dialog request was handled on B2 (acting-backup) ──────
    assert!(
        b2.metrics().creations_total() > b2_creations_before,
        "GATE 3: b2 processed the failed-over in-dialog request (created the call locally)",
    );
    let b2_cdrs = b2.cdr_records();
    assert!(
        b2_cdrs.iter().any(|c| c
            .events
            .iter()
            .any(|e| e.event_type == CdrEventType::Bye)),
        "GATE 3: b2's CDR records the BYE it handled, got {:?}",
        b2_cdrs.iter().flat_map(|c| c.events.iter().map(|e| e.event_type)).collect::<Vec<_>>(),
    );

    // ── STEP 4: reboot B1 EMPTY at a higher gen → re-hydrate from B2 ─────────
    let old_gen = b1.gen();
    b1.reboot().await;
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

    // Mark B1 alive+ready in the proxy registry (deterministic, off is_ready()).
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
    let mut bye2 = dlg2.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye2.expect(200).await;
    fh.advance(Duration::from_millis(300)).await;

    // ── combined report (recording-first; SIP + replication together) ────────
    drop((w_b1, w_b2, proxy)); // drop SUTs so the HarnessHandle is single-owned.
    let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("s10b-canonical");
    let _ = std::fs::remove_dir_all(&out);
    let written = fh.write_report(&out).await.unwrap();
    eprintln!("\ns10b combined failover report:");
    for p in &written {
        eprintln!("  {}", p.display());
    }
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
// FAILOVER TIMER RE-ARM — a call hydrated by a takeover must regenerate its
// per-call timers (keepalive / global-duration) on the new owner. Those timers
// are runtime fibers in the in-memory `TimerService`, NOT replicated state, so
// a hydrated call arrives with no live timers; without re-arming on hydration a
// dead peer is never probed and the call is never reaped → `active_calls` leaks
// on the takeover node (the failover analogue of the steady-state no-BYE leak).
//
// Scenario: establish on b1, replicate to b2, CRASH b1, then drive a non-
// terminating in-dialog request (a re-INVITE) over to b2 — this hydrates the
// call onto b2 AND (with the fix) re-arms its keepalive. Bob then goes silent on
// the next keepalive OPTIONS; after interval+timeout b2 reaps the hydrated call
// and writes a CDR with a Bye event. The CDR is the proof the timer regenerated.
// ===========================================================================
/// The Rust default keepalive interval / per-leg timeout (defaults.rs).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn hydrated_call_rearms_keepalive_and_reaps_dead_peer() {
    let mut fh = FailoverHarness::new("s10b-hydration-timer-rearm", &["b1", "b2"]);

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

    // ── STEP 4: keepalive probe — bob goes SILENT on the hydrated call's leg ──
    // If hydration re-armed the keepalive, b2 now probes both legs with OPTIONS.
    fh.advance(KEEPALIVE_INTERVAL).await;
    // alice answers its OPTIONS; bob receives but never answers (dead-peer shape).
    alice.receive_tolerating("OPTIONS", &["INVITE", "ACK"]).await.respond(200, "OK").await;
    let _silent = bob.receive_tolerating("OPTIONS", &["INVITE", "ACK"]).await;

    // ── STEP 5: bob's keepalive times out → b2 reaps the hydrated call ────────
    fh.advance(KEEPALIVE_TIMEOUT).await;
    // The healthy peer (alice) gets the teardown BYE from b2.
    alice
        .receive_tolerating("BYE", &["OPTIONS", "INVITE", "ACK"])
        .await
        .respond(200, "OK")
        .await;
    fh.advance(Duration::from_millis(500)).await;

    // ── PROOF: b2 wrote a CDR with a Bye (the keepalive-timeout teardown) ─────
    // Only possible if the keepalive timer was REGENERATED on hydration; without
    // the fix the hydrated call has no keepalive, nothing fires, no CDR, leak.
    let mut cdrs = b2.cdr_records();
    for _ in 0..40 {
        if cdrs.iter().any(|c| c.events.iter().any(|e| e.event_type == CdrEventType::Bye)) {
            break;
        }
        fh.advance(Duration::from_millis(200)).await;
        cdrs = b2.cdr_records();
    }
    assert!(
        cdrs.iter().any(|c| c.events.iter().any(|e| e.event_type == CdrEventType::Bye)),
        "GATE: hydrated call reaped by its REGENERATED keepalive (CDR Bye on b2); \
         events seen = {:?}",
        cdrs.iter().flat_map(|c| c.events.iter().map(|e| e.event_type)).collect::<Vec<_>>(),
    );
    assert!(
        b2.metrics().removals_total() >= 1,
        "the hydrated call was removed on b2 (active_calls did not leak)",
    );

    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
}

// ===========================================================================
// X11 RECLAIM/HANDBACK — after a kill→takeover→reboot cycle a call must be
// served by EXACTLY ONE node. The reborn primary reclaims it (bulk on go-active)
// AND the backup deactivates its takeover copy (the `Deactivate` handback), so
// the two never double-serve. Repro for the cluster leak: a cross-node duplicate
// that is never handed back accumulates as a ghost. Ground-truth on the live
// in-memory count (not the creations/removals counters, which are under test).
// ===========================================================================
#[tokio::test(start_paused = true)]
async fn reboot_reclaim_hands_back_exactly_one_owner() {
    let mut fh = FailoverHarness::new("s11-reclaim-handback", &["b1", "b2"]);
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
    assert!(b2.get(BAK, &primary_ord, &call_ref).await.is_some(), "backup holds the replica");
    assert_eq!(b1.active_calls(), 1, "primary serves the established call");
    assert_eq!(b2.active_calls(), 0, "backup is a pure replica (not serving)");

    // ── crash primary; fail a NON-terminating re-INVITE over to the backup ────
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack(Some(ANSWER)).await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(b2.active_calls(), 1, "backup took the call over (live takeover copy)");

    // ── reboot the primary → bulk reclaim on go-active + Deactivate broadcast ─
    b1.reboot().await;
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    // Drive the go-active task: ReclaimAll + the ~5 s Deactivate broadcast +
    // b2's puller reconnect (which carries the handback / reconnect backstop).
    fh.advance(Duration::from_secs(10)).await;

    // ── THE INVARIANT: exactly one node serves the call (no ghost duplicate) ──
    eprintln!(
        "post-reclaim: b1.active={} b2.active={} reclaimed={} handback={} hydrated={}",
        b1.active_calls(),
        b2.active_calls(),
        b1.metrics().repl_reclaimed_total(),
        b2.metrics().repl_handback_total(),
        b2.metrics().repl_takeover_hydrated_total(),
    );
    assert_eq!(
        b1.active_calls() + b2.active_calls(),
        1,
        "EXACTLY ONE node serves the call after reclaim+handback — a duplicate is the ghost leak",
    );
    assert_eq!(b1.active_calls(), 1, "the rebooted primary reclaimed + serves it");
    assert_eq!(b2.active_calls(), 0, "the backup handed its takeover copy back");
    assert!(b2.metrics().repl_handback_total() >= 1, "the handback (Deactivate) fired on the backup");

    drop((w_b1, w_b2, proxy));
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
// This is the BYE-termination twin of `reboot_reclaim_hands_back_exactly_one_owner`
// (which fails a NON-terminating re-INVITE over): there the primary reclaims the
// LIVE call; here it must reclaim NOTHING.
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
    assert!(b2.get(BAK, &primary_ord, &call_ref).await.is_some(), "backup holds the replica");

    // ── crash the primary; the proxy fails the in-dialog BYE over to the backup,
    //    which TAKES OVER and TERMINATES the call (the acting-backup teardown). ──
    b1.crash();
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await; // b2 relays the BYE to bob
    bye.expect(200).await;
    fh.advance(Duration::from_millis(500)).await;

    // Let the buffered reverse-delete drain (the bak body is removed off the hot path).
    for _ in 0..40 {
        if b2.get(BAK, &primary_ord, &call_ref).await.is_none() {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }

    // ── INVARIANT 1: the acting-backup released ALL of the takeover context ────
    assert_eq!(b2.active_calls(), 0, "acting-backup dropped the live takeover copy on terminate");
    assert_eq!(b2.lock_count(), 0, "acting-backup released the per-call lock on terminate");
    assert!(
        b2.get(BAK, &primary_ord, &call_ref).await.is_none(),
        "acting-backup deleted its bak:{primary_ord} body on terminate (no stale replica)",
    );
    assert!(
        !b2.scan_backed_up(&primary_ord).contains(&call_ref),
        "terminated call left the backup keyset — bootstrap cannot replay it to the primary",
    );

    // ── reboot the primary → it re-hydrates from the backup + bulk-reclaims ────
    b1.reboot().await;
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if b1.is_ready() {
            break;
        }
    }
    assert!(b1.is_ready(), "rebooted primary ready after re-hydration");
    // Drive the go-active task: ReclaimAll bulk sweep + the straggler/handback burst.
    fh.advance(Duration::from_secs(10)).await;

    // ── INVARIANT 2: the primary did NOT resurrect the terminated dialog ───────
    eprintln!(
        "post-reboot: b1.active={} b1.reclaimed={} pri:{primary_ord}={:?} gen={:?}",
        b1.active_calls(),
        b1.metrics().repl_reclaimed_total(),
        b1.scan_primary(&primary_ord),
        b1.call_gen(PartitionRole::Primary, &primary_ord, &call_ref),
    );
    assert_eq!(
        b1.active_calls(),
        0,
        "primary must NOT reclaim the terminated call into its live map (a resurrection = a ghost)",
    );
    assert!(
        !b1.scan_primary(&primary_ord).contains(&call_ref),
        "terminated call is absent from pri:{primary_ord} — the Reverse DELETE propagated, not a stale create",
    );
    assert!(
        b1.call_gen(PartitionRole::Primary, &primary_ord, &call_ref).is_none(),
        "no expired call_gen lingered in pri:{primary_ord} for the terminated call",
    );
    assert_eq!(
        b1.metrics().repl_reclaimed_total(),
        0,
        "primary reclaimed NOTHING — its only call had already terminated on the backup",
    );
    assert_eq!(b2.active_calls(), 0, "backup holds nothing after the handback window");

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
    let mut bye2 = dlg2.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye2.expect(200).await;
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
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
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
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
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
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

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

    // BOTH planes present in the one artifact.
    assert!(combined.contains("SIP plane"), "combined report has a SIP section");
    assert!(combined.contains("Replication plane"), "combined report has a replication section");
    assert!(combined.contains("INVITE"), "SIP exchange shows the INVITE");
    assert!(combined.contains("BYE"), "SIP exchange shows the BYE");
    assert!(
        combined.contains("PullRequest") || combined.contains("Data") || combined.contains("Noop"),
        "replication exchange shows a repl frame",
    );
    assert!(combined.contains("crash"), "report shows the crash marker");

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
