//! ADR-0023 — the injectable store-fault seam and its defined store-failure
//! semantics, driven end-to-end through a real `B2buaCore`.
//!
//! One `StoreFaults` handle (injected via `B2buaSutBuilder::with_store_faults`)
//! controls both halves of the seam: the `FaultInjectingCallStore` decorator
//! and the router's live-path probe. These tests pin the three defined
//! semantics against the three live probe points:
//!
//!  1. **Initial INVITE** (`LiveInitialInvite`): fail CLOSED — the caller who
//!     heard the auto-100 gets a final **500** (ADR-0022 composition), no call
//!     is created, nothing leaks.
//!  2. **In-dialog request** (`LiveInDialog`): fail CLOSED — the BYE gets a
//!     **500**, the call and its state stay untouched (distinct from the 481
//!     lookup-MISS), and a retry after the store recovers proceeds normally.
//!  3. **Audit/keepalive** (`LiveAudit`): fail OPEN — the probe cycle is
//!     skipped, the established call SURVIVES, and the keepalive timer is
//!     RE-ARMED so liveness detection resumes next interval (protected-calls
//!     invariant, docs/testing/ha-acceptance.md).

use std::time::Duration;

use b2bua::store::{StoreFaultPoint, StoreFaults};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, OFFER_SDP};

/// The harness-default keepalive interval (`B2buaSutBuilder::start` pins 30 s).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Scene with the fault handle injected: alice ↔ b2bua(→bob) ↔ bob at the
/// canonical ports, the b2bua's store wrapped in the fault decorator and its
/// live-path probe riding the same `StoreFaults` clone the test retains.
async fn scene_with_faults(name: &str, faults: &StoreFaults) -> B2buaScene {
    let faults = faults.clone();
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).with_store_faults(faults)
    })
    .await
}

/// (1) Fault armed from the start: alice's initial INVITE draws the auto-100
/// and then the fail-closed **500** final — never 100-then-silence (ADR-0022
/// composition) — with NO call created and nothing leaked.
#[tokio::test(start_paused = true)]
async fn initial_invite_store_fault_fails_closed_500_no_call_created() {
    let faults = StoreFaults::default();
    faults.arm(StoreFaultPoint::LiveInitialInvite);
    let s = scene_with_faults("store-fault-invite", &faults).await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(500)).await;
    call.expect(500).await;

    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "fail-closed: no b-leg INVITE was ever routed",
    );
    assert_eq!(s.b2bua.metrics().store_fault_rejected_total(), 1, "the reject is metered");
    assert!(s.b2bua.cdr_records().is_empty(), "no call was created — no CDR");

    // Nothing leaked: the dispatch created a per-call queue for the rejected
    // INVITE; the orphan teardown must balance it (creations == removals,
    // no stranded lock/stamp).
    settle_until(|| {
        s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total()
    })
    .await;
    s.b2bua.assert_fully_reaped();

    let _report = s.finish().await;
}

/// (2) Mid-call fault on the in-dialog path: the BYE draws a **500** and the
/// call stays untouched; after the store "recovers" (disarm) the re-sent BYE
/// tears the call down normally — the retry contract.
#[tokio::test(start_paused = true)]
async fn in_dialog_bye_store_fault_500_then_retry_succeeds() {
    let faults = StoreFaults::default();
    let s = scene_with_faults("store-fault-bye", &faults).await;

    let mut dialog = s.establish().await;

    // Store goes down mid-call.
    faults.arm(StoreFaultPoint::LiveInDialog);
    let mut bye1 = dialog.bye().await;
    bye1.expect(500).await;
    assert!(
        s.bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the faulted BYE was answered locally, never relayed",
    );
    assert_eq!(s.b2bua.metrics().store_fault_rejected_total(), 1, "the 500 is metered");
    assert_eq!(s.b2bua.active_calls(), 1, "the call and its state stay untouched");

    // Store recovers; the retried BYE (next CSeq) proceeds normally.
    faults.disarm(StoreFaultPoint::LiveInDialog);
    let mut bye2 = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye2.expect(200).await;

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    assert_eq!(
        s.b2bua.metrics().store_fault_rejected_total(),
        1,
        "only the armed-window BYE was rejected",
    );

    let _report = s.finish().await;
}

/// (3) Fault on the audit path: the keepalive cycle at t+30 s is skipped FAIL-
/// OPEN — no OPTIONS probe, the established call SURVIVES — and the timer is
/// re-armed, so after disarming the next cycle at t+60 s probes both legs
/// normally. A store fault alone never tears down an established call.
#[tokio::test(start_paused = true)]
async fn keepalive_audit_store_fault_fails_open_and_rearms() {
    let faults = StoreFaults::default();
    let s = scene_with_faults("store-fault-audit", &faults).await;

    let mut dialog = s.establish().await;

    // Store down for exactly one audit cycle.
    faults.arm(StoreFaultPoint::LiveAudit);
    s.h.advance(KEEPALIVE_INTERVAL).await;
    assert!(
        s.alice.try_receive_tolerating("OPTIONS", &[]).await.is_none(),
        "skipped cycle: no OPTIONS probe to alice",
    );
    assert!(
        s.bob.try_receive_tolerating("OPTIONS", &[]).await.is_none(),
        "skipped cycle: no OPTIONS probe to bob",
    );
    assert_eq!(
        s.b2bua.metrics().store_fault_audit_skipped_total(),
        1,
        "the skipped cycle is metered",
    );
    assert_eq!(s.b2bua.active_calls(), 1, "the call SURVIVES the skipped audit cycle");

    // Store recovers: the RE-ARMED timer fires the next interval and the
    // keepalive proceeds normally (OPTIONS out, 200s absorbed).
    faults.disarm(StoreFaultPoint::LiveAudit);
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;
    assert_eq!(
        s.b2bua.metrics().store_fault_audit_skipped_total(),
        1,
        "no further cycle was skipped after recovery",
    );

    // Normal teardown; fully reaped.
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();

    let _report = s.finish().await;
}
