//! Regression: an **orphan-reject (481) must not leak per-call dispatch state**.
//!
//! When an in-dialog request resolves to a callRef but hydrates **no** live call
//! (`hydrate_from_replica` → None), the B2BUA answers `481 Call/Transaction Does
//! Not Exist`. Before the fix that path `return`ed without tearing down the
//! per-call state the dispatch had just created — the per-call queue + its idle
//! worker task, the `creations` bump (→ `b2bua_active_calls`), and the per-call
//! serialization lock (→ `b2bua_store_locks`). A per-call worker exits ONLY on
//! poison, so every orphan stranded one of each, *permanently*.
//!
//! In the cluster this was invisible per-call but catastrophic in aggregate: a
//! `kill_worker` left ~3000 long dialogs un-reclaimed on the rebooted worker;
//! their UACs timed out and BYE'd; each BYE resolved (callRef in the in-dialog
//! R-URI) → 481 → one leaked queue+lock. The rebooted worker's `active_calls`
//! and `store_locks` ratcheted ~3150 above its TRUE call-map size and never
//! drained — the "leak detector" panel — even after all traffic stopped
//! (`store_calls`=0 but `active_calls`≈`store_locks`≈3000 post-drain).
//!
//! Repro: establish several independent calls, tear each down (distinct Call-IDs
//! ⇒ distinct callRefs ⇒ a leak shows N-fold), then fire ONE in-dialog BYE at
//! each now-dead dialog (each → 481). The invariant is the memory's post-drain
//! rule: once traffic drains, the per-call accounting returns to ZERO —
//! `creations == removals` (⇒ `active_calls` 0) and `lock_count` 0.

use std::time::Duration;

use b2bua_harness::B2buaSut;
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Independent dialogs to establish, kill, and then orphan. >1 so a leak shows
/// N-fold (the production ratchet was thousands of distinct lost callRefs).
const LOST_DIALOGS: usize = 6;

#[tokio::test(start_paused = true)]
async fn orphan_in_dialog_481_does_not_leak_dispatch_state() {
    let h = Harness::new("b2bua-orphan-reject-no-leak");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;
    let b2bua = B2buaSut::route_all_to(&h, "b2bua", "127.0.0.1:5086", "127.0.0.1", 5076).await;

    // ── Establish N independent calls and tear each down, KEEPING each dialog so
    //    we can orphan it. Each in-dialog R-URI carries the B2BUA's callRef (the
    //    production in-dialog routing key), so a later BYE resolves to a callRef
    //    whose call is gone — exactly the failed-over-BYE shape.
    let mut dead_dialogs = Vec::with_capacity(LOST_DIALOGS);
    for _ in 0..LOST_DIALOGS {
        let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
        let mut uas = bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        bob.receive("ACK").await;

        // Tear it down: the B2BUA removes the call AND poisons its dispatch queue.
        let mut bye = dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        dead_dialogs.push(dialog);
    }

    // Let every teardown settle — the poison is async (the worker drains+exits).
    settle_until(&h, || {
        b2bua.metrics().removals_total() == b2bua.metrics().creations_total()
            && b2bua.lock_count() == 0
    })
    .await;

    let base_creations = b2bua.metrics().creations_total();
    assert_eq!(base_creations, LOST_DIALOGS as u64, "served N real calls");
    assert_eq!(b2bua.metrics().removals_total(), LOST_DIALOGS as u64, "tore N real calls down");
    assert_eq!(b2bua.active_calls(), 0, "no live call after the teardowns");
    assert_eq!(b2bua.lock_count(), 0, "no lock survives a clean teardown");

    // ── Fire ONE in-dialog BYE at each now-dead dialog. Each resolves to a gone
    //    callRef → `hydrate_from_replica` misses → 481. Pre-fix each leaks a
    //    queue + lock + an unmatched creation. (Distinct callRefs, so they never
    //    contend on one queue.)
    for dialog in dead_dialogs.iter_mut() {
        let mut orphan = dialog.send_request(InDialogMethod::Bye).send().await;
        orphan.expect(481).await;
    }

    // Let the orphan teardowns settle.
    settle_until(&h, || {
        b2bua.metrics().removals_total() == b2bua.metrics().creations_total()
            && b2bua.lock_count() == 0
    })
    .await;

    // PROOF the orphans actually exercised the dispatch path: one fresh per-call
    // queue (creation) per orphan, spun up before the call was found to be gone.
    assert_eq!(
        b2bua.metrics().creations_total(),
        base_creations + LOST_DIALOGS as u64,
        "each orphan in-dialog request created a per-call dispatch queue",
    );

    // ── THE INVARIANT: drained ⇒ per-call accounting back to ZERO. Pre-fix this
    //    fails — `removals` stays at N while `creations` is 2N, and `lock_count`
    //    sits at N (one stranded lock per orphan callRef).
    b2bua.assert_fully_reaped();
}

/// Spin the paused clock in small steps until `cond` holds — the dispatcher's
/// poison drain is task-scheduled (not time-driven), so a brief settle lets the
/// worker exit and bump `removals`. Bounded so a real regression (the condition
/// never holds) falls through to the assertions instead of hanging.
async fn settle_until(h: &Harness, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        h.advance(Duration::from_millis(5)).await;
    }
}
