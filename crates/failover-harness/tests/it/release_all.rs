//! **A call an operator released leaves its backup nothing to take over.** The
//! release-all of the call's primary (ADR-0050 §10) closes its record and
//! sends nothing to either party; its delete reaches the backup like any
//! release's, so the backup holds no replica of it. A later request in the
//! dialog draws `481` (RFC 3261 §12.2.2) from the primary while it lives, and
//! from the backup once the primary is gone: no takeover, no second record.
//!
//! ```text
//!   answered call · primary serves · backup synchronized
//!   operator ──release-all──▶ primary   (record closed, nothing sent, replica deleted)
//!   alice ──BYE──▶ proxy ──▶ primary ──▶ 481
//!                    ✗ primary killed ✗
//!   bob   ──BYE──▶ proxy ──▶ backup  ──▶ 481
//! ```
//!
//! The parties are left holding a dialog the SUT dropped: that is what the
//! test asserts. Each ends its side with a BYE, and the `481` terminates it
//! (RFC 3261 §12.2.1.2).

use std::time::Duration;

use b2bua::release_all::{ReleasedAll, TOPIC};
use b2bua::CallEvent;
use failover_harness::{assert_call_fully_released, total_cdrs_for, FailoverHarness, WorkerHealth};

use crate::new_call_acceptance::{answered, bring_up, call_ref_of, caller_hangs_up, drain};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

#[tokio::test(start_paused = true)]
async fn a_released_call_is_gone_from_its_backup_and_a_later_bye_draws_481_on_either_worker() {
    let mut fh = FailoverHarness::new("release-all-leaves-the-backup-nothing", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let mut c = bring_up(&mut fh).await;

    let (mut dialog, mut bob_dialog, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    let call_ref = call_ref_of(&fh, c.node(&bak), &pri).await;
    let takeovers = c.takeovers();

    // ── the operator releases every call of the primary ─────────────────────
    let mark = fh.sip_entries().len();
    fh.mark(&pri, None, "release-all", "the operator releases the primary's calls");
    assert_eq!(c.node(&pri).release_all(), ReleasedAll { released: 1, aborted: 0 });
    fh.advance(Duration::from_millis(500)).await;
    let parties = [alice.addr(), bob.addr()];
    let toward_parties =
        fh.sip_entries().into_iter().skip(mark).filter(|e| parties.contains(&e.to)).count();
    assert_eq!(toward_parties, 0, "nothing is sent to either party");
    assert!(!c.node(&pri).serves(&call_ref), "the primary dropped the call");
    assert!(!c.node(&bak).holds_any_trace(&call_ref).await, "the backup holds no replica");
    assert_eq!(total_cdrs_for(&c.nodes(), &call_ref), 1, "the release closed the record");

    // ── the caller's BYE reaches the primary ────────────────────────────────
    dialog.bye().await.expect(481).await;

    // ── the primary dies; the callee's BYE reaches the backup ───────────────
    fh.mark(&pri, None, "crash", "the primary dies after the release");
    let primary = if pri == "b1" { &mut c.b1 } else { &mut c.b2 };
    primary.crash();
    c.proxy.set_health(&pri, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    bob_dialog.bye().await.expect(481).await;

    drain(&fh, &c).await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "the backup took nothing over");
    assert_eq!(total_cdrs_for(&c.nodes(), &call_ref), 1, "one record for the call");
    assert_call_fully_released(&c.nodes(), &call_ref).await;
}

/// A release reaches a call only while the worker serves it: the turn of a
/// call that left the worker between the request and its turn (here, one the
/// worker never served, only backs up) takes nothing over from the replica
/// store and ends nothing. The call plays on at its primary and ends by its
/// BYE.
#[tokio::test(start_paused = true)]
async fn a_release_for_a_call_the_worker_does_not_serve_takes_nothing_over() {
    let mut fh = FailoverHarness::new("release-all-reaches-served-calls-only", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let c = bring_up(&mut fh).await;

    let (mut dialog, _bob_dialog, (pri, bak)) = answered(&fh, &alice, &bob, &c.proxy).await;
    let call_ref = call_ref_of(&fh, c.node(&bak), &pri).await;
    let incarnation = c.node(&pri).live_call(&call_ref).map(|call| call.incarnation().to_string());
    let takeovers = c.takeovers();

    c.node(&bak).post_event(CallEvent::InternalEvent {
        call_ref: call_ref.clone(),
        topic: TOPIC.to_string(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation,
    });
    fh.advance(Duration::from_millis(500)).await;
    assert!(!c.node(&bak).serves(&call_ref), "the backup serves nothing");
    assert_eq!(c.takeovers(), takeovers, "the backup took nothing over");
    assert!(c.node(&pri).serves(&call_ref), "the primary serves the call");
    assert_eq!(total_cdrs_for(&c.nodes(), &call_ref), 0, "the call is not over");

    caller_hangs_up(&mut dialog, &bob).await;
    drain(&fh, &c).await;
    assert_eq!(total_cdrs_for(&c.nodes(), &call_ref), 1, "one record, its BYE's");
    assert_call_fully_released(&c.nodes(), &call_ref).await;
}
