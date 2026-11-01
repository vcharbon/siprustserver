//! **A dead verdict told to a backed-up worker is taken at once, each verdict
//! once, in the order told.** The backup of an established call has its
//! router loop slowed to [`PER_TURN`] a turn behind a backlog longer than the
//! notice freshness window. The proxy reads the call's live primary dead, alive
//! again, then dead once more, telling each verdict on an OPTIONS (ADR-0050
//! §3): the backup takes the first before the backlog has drained, and the
//! second once told; neither the notices' repeats nor the backlog's wait make
//! one refused. The replication stream from the primary lives, so nothing is
//! taken over (ADR-0047 D1). The call ends with the caller's BYE and one CDR.
//!
//! Beyond the failover sweep: the dead notice on a starved worker.

use std::time::Duration;

use b2bua_harness::{drop_backlog_answers, send_backlog, Pace, BACKLOG_METHOD};
use failover_harness::{assert_call_fully_over, WorkerHealth};
use tokio::time::Instant;

use crate::call_terminate_on_backup::{establish, Established};

const CAROL: &str = "127.0.0.1:5062";

/// The simulated time each turn of the backup's router costs.
const PER_TURN: Duration = Duration::from_millis(100);
/// The backlog: fifteen seconds of turns, past the 10 s freshness window.
const BACKLOG: usize = 150;
/// The time a verdict told to the backup takes to be taken: the notice's
/// flight and the turn in progress.
const TOLD: Duration = Duration::from_secs(1);

#[tokio::test(start_paused = true)]
async fn a_dead_verdict_behind_a_backlog_is_taken_once_in_the_order_told() {
    let name = "dead-notice-behind-a-backlog";
    let Established {
        fh,
        alice,
        bob,
        proxy,
        store,
        lh: _lh,
        w_b1,
        w_b2,
        primary_ord,
        call_ref,
        mut dialog,
        ..
    } = establish(name).await;
    let backup = if primary_ord == "b1" { &w_b2 } else { &w_b1 };
    let carol = fh.agent_with_pre_ingress("carol", CAROL, drop_backlog_answers()).await;
    let pace = fh.turn_points().router_pace().clone();
    pace.set(backup.ordinal(), Pace::PerTurn(PER_TURN));
    send_backlog(&carol, backup.sip_addr(), BACKLOG, "backlog").await;
    let refused = || backup.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);
    fh.advance(Duration::from_millis(500)).await;

    // ── the first verdict ───────────────────────────────────────────────────
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(TOLD).await;
    assert_eq!(
        backup.metrics().dead_verdicts_taken_total(),
        1,
        "the verdict is taken ahead of the backlog"
    );
    assert!(refused() < BACKLOG as u64, "the backlog is still queued");

    // ── read alive, then dead again: a second verdict ───────────────────────
    proxy.set_health(&primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;
    proxy.set_health(&primary_ord, WorkerHealth::Dead);
    fh.advance(TOLD).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 2, "the second verdict is taken");
    proxy.set_health(&primary_ord, WorkerHealth::Alive);

    // ── the backlog drains: every repeat was a copy, none refused ───────────
    let give_up = Instant::now() + Duration::from_secs(20);
    while refused() < BACKLOG as u64 && Instant::now() < give_up {
        fh.advance(Duration::from_millis(500)).await;
    }
    assert_eq!(refused(), BACKLOG as u64, "every backlog request is refused 405");
    pace.set(backup.ordinal(), Pace::Free);
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 2, "each verdict taken once");
    assert_eq!(backup.metrics().dead_notices_refused_total(), 0, "no notice refused");
    assert!(!backup.serves(&call_ref), "a live primary's call is not taken over");

    // ── the call ends by the caller's BYE ───────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the call drains cleanly");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    fh.assert_sip_rfc_clean(name);
}
