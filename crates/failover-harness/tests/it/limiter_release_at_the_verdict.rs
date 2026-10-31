//! **The limiter holds of an end deferred to a primary read dead leave at the
//! confirmed verdict; the CDR waits for the primary.** A takeover copy that ends
//! a call while its primary is a member defers the CDR to it (ADR-0020 X3). Once
//! a dead verdict on the primary acted here, confirmed on the replication link,
//! the copy's end releases the call's limiter key, and the verdict's sweep
//! releases the key of every end deferred before it (ADR-0047 D8). The
//! release is keyed by the call, so the departure's, the reclaim's or a live
//! primary's own release frees nothing more: each id carries a witness hold
//! that a surplus release would take.
//!
//! ```text
//!   P crashes · the proxy reads it dead · B confirms on the stream
//!   alice ──BYE──▶ proxy ──▶ B   (ends the call, defers the CDR)
//!   the hold leaves at the verdict, or at the end when it comes later
//!   P departs, or reboots and reclaims: the one CDR, no second release
//! ```
//!
//! A verdict on a live primary behind a replication partition is a misread
//! the stream cannot tell: the copy's end releases the hold, the primary's
//! refreshes are refused by the release fence, and the primary writes the one
//! CDR when the partition heals. A verdict the stream does not confirm
//! releases nothing.
//!
//! Beyond the failover sweep: the limiter hold timing of a deferred end.

use std::time::Duration;

use b2bua::limiter::RefreshOutcome;
use failover_harness::{
    assert_call_fully_over, cookie_field, total_cdrs_for, ReplicatedB2buaSut, WorkerHealth,
};

use crate::limiter_release_by_call::{limited_decision, spawn_workers, LimiterRig};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// Longer than the replication idle bound (`B2BUA_REPL_IDLE_TIMEOUT_MS`): a
/// verdict on a silent primary is confirmed within it.
const CONFIRMED_WITHIN: Duration = Duration::from_secs(3);

/// An established call holding `[x, y]` on a primary backed by the other
/// worker, replicated.
struct Placed {
    fh: failover_harness::FailoverHarness,
    alice: scenario_harness::Agent,
    bob: scenario_harness::Agent,
    rig: LimiterRig,
    proxy: failover_harness::ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    pri_ord: String,
    bak_ord: String,
    call_ref: String,
    dialog: scenario_harness::Dialog,
}

async fn place(name: &str) -> Placed {
    let mut fh = failover_harness::FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve().await;
    let (proxy, w_b1, w_b2) = spawn_workers(&mut fh, &rig, limited_decision(&["x", "y"])).await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let pri_ord = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [1, 1, 0], "the call holds x and y");
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" }.to_string();
    let backup = if pri_ord == "b1" { &w_b2 } else { &w_b1 };
    let mut call_ref = None;
    for _ in 0..50 {
        call_ref = backup.scan_one_backed_up(&pri_ord).await;
        if call_ref.is_some() {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    let call_ref = call_ref.expect("the backup holds the replicated call");
    Placed { fh, alice, bob, rig, proxy, w_b1, w_b2, pri_ord, bak_ord, call_ref, dialog }
}

/// The primary's host is lost with its sockets open, its replication stream
/// connected and silent, and the proxy reads it dead; the caller's BYE
/// reaches the backup before the stream confirms the verdict at its idle
/// bound. The copy ends the call and defers it, its holds kept; the confirmed
/// verdict releases them, with no CDR. The primary departs: the sweep writes
/// the one CDR, and its release frees nothing more.
#[tokio::test(start_paused = true)]
async fn an_end_deferred_before_the_verdict_releases_its_holds_at_the_confirmed_verdict() {
    let Placed {
        mut fh,
        alice,
        bob,
        rig,
        proxy,
        mut w_b1,
        mut w_b2,
        pri_ord,
        bak_ord,
        call_ref,
        mut dialog,
    } = place("limiter-release-at-the-verdict").await;
    let (primary, backup) = if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    fh.partition(&pri_ord, &bak_ord);
    fh.cut_signalling(&pri_ord, primary.sip_addr());
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(100)).await;
    assert_eq!(rig.holds(), [1, 1, 0], "the end defers its holds: the verdict waits on the stream");
    assert!(backup.holds_any_trace(&call_ref).await, "the backup keeps the ended call");

    fh.advance(CONFIRMED_WITHIN).await;
    assert_eq!(backup.metrics().dead_verdicts_unconfirmed_total(), 0, "the stream confirmed it");
    assert_eq!(rig.holds(), [0, 0, 0], "the confirmed verdict released the deferred holds");
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the CDR still waits for the primary");
    assert!(backup.holds_any_trace(&call_ref).await, "and so does the ended call");
    let released_at_the_verdict = rig.store.stats().releases_total;

    fh.mark(&pri_ord, None, "crash", "the lost host's process ends");
    primary.crash();
    fh.depart(&pri_ord);
    let discharged = fh
        .settle_terminal(async || {
            total_cdrs_for(&[backup], &call_ref) == 1 && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(discharged, "the departure's sweep discharged the deferred end");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        rig.store.stats().releases_total,
        released_at_the_verdict + 1,
        "the discharge releases the key again"
    );
    assert_eq!(rig.holds(), [0, 0, 0], "which frees nothing more: the witnesses are intact");
    rig.release_witnesses();
    assert_call_fully_over(&[backup], &call_ref, &rig.store).await;
    drop(proxy);
}

/// The verdict on the crashed primary is confirmed while the call is quiet:
/// the call stays counted. The caller's BYE then ends it on the backup, which
/// releases the holds at once and defers the CDR. The primary reboots inside
/// the replica TTL and reclaims the ended call: the one CDR, and its release
/// frees nothing more.
#[tokio::test(start_paused = true)]
async fn an_end_after_the_confirmed_verdict_releases_its_holds_at_once() {
    let Placed {
        mut fh,
        alice,
        bob,
        rig,
        proxy,
        mut w_b1,
        mut w_b2,
        pri_ord,
        bak_ord,
        call_ref,
        mut dialog,
    } = place("limiter-release-after-the-verdict").await;
    let (primary, backup) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(CONFIRMED_WITHIN).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    assert_eq!(backup.metrics().dead_verdicts_unconfirmed_total(), 0, "and confirmed");
    assert_eq!(rig.holds(), [1, 1, 0], "the quiet call stays counted");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the end released the holds");
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the CDR waits for the primary");
    assert!(backup.holds_any_trace(&call_ref).await, "and so does the ended call");
    let released_at_the_end = rig.store.stats().releases_total;

    let new_addr = primary.reboot().await;
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "the rebooted primary re-hydrated from the backup");
    proxy.set_address(&pri_ord, new_addr);
    fh.note_worker_rebound(&pri_ord, new_addr);
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let reclaimed = fh
        .settle_terminal(async || {
            total_cdrs_for(&[primary], &call_ref) == 1
                && !primary.holds_any_trace(&call_ref).await
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(reclaimed, "the reclaim discharged the deferred end on the primary {pri_ord}");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(
        rig.store.stats().releases_total,
        released_at_the_end + 1,
        "the reclaim releases the key again"
    );
    assert_eq!(rig.holds(), [0, 0, 0], "which frees nothing more: the witnesses are intact");
    assert_eq!(total_cdrs_for(&[backup], &call_ref), 0, "the backup {bak_ord} writes none");
    rig.release_witnesses();
    assert_call_fully_over(&[primary, backup], &call_ref, &rig.store).await;
    drop(proxy);
}

/// The proxy reads a live primary dead while the replication link between the
/// two workers is cut both ways: the stream falls silent and confirms the
/// misread. The caller's BYE ends the call on the backup, which releases the
/// holds and defers the CDR. The live primary's refresh lands on the release
/// fence and counts nothing again. The partition heals: the primary reconciles
/// the backup's ended call and writes the one CDR, and its release frees
/// nothing more.
#[tokio::test(start_paused = true)]
async fn a_misread_verdict_behind_a_partition_keeps_one_cdr_and_the_count() {
    let Placed {
        mut fh,
        alice,
        bob,
        rig,
        proxy,
        w_b1,
        w_b2,
        pri_ord,
        bak_ord,
        call_ref,
        mut dialog,
    } = place("limiter-release-at-a-misread-verdict").await;
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    fh.partition(&pri_ord, &bak_ord);
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(CONFIRMED_WITHIN).await;
    assert_eq!(backup.metrics().dead_verdicts_unconfirmed_total(), 0, "the silent stream confirms");
    assert_eq!(rig.holds(), [1, 1, 0], "the quiet call stays counted");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the backup's end released the holds");
    assert_eq!(total_cdrs_for(&[primary, backup], &call_ref), 0, "the CDR waits for the primary");

    let refresh = b2bua::B2buaConfig::default().limiter_refresh_sec as u64;
    fh.advance(Duration::from_secs(refresh + 5)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the live primary's refresh counts nothing again");
    assert!(
        primary.metrics().limiter().refresh_answers_total(RefreshOutcome::Released) >= 1,
        "its refresh met the release fence"
    );

    fh.heal(&pri_ord, &bak_ord);
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let reconciled = fh
        .settle_terminal(async || {
            total_cdrs_for(&[primary], &call_ref) == 1
                && !primary.holds_any_trace(&call_ref).await
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(reconciled, "the primary reconciled the backup's end");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "no release took a witness");
    rig.release_witnesses();
    assert_call_fully_over(&[primary, backup], &call_ref, &rig.store).await;
    drop(proxy);
}

/// The proxy reads a live primary dead while its frames still reach the
/// backup: the verdict is not confirmed. The caller's BYE ends the call on the
/// backup while the backup's writes to the primary are held: the copy defers
/// and releases nothing. The writes flow again; the primary reconciles the end:
/// one CDR, one release.
#[tokio::test(start_paused = true)]
async fn an_unconfirmed_verdict_releases_nothing() {
    let Placed {
        mut fh,
        alice,
        bob,
        rig,
        proxy,
        w_b1,
        w_b2,
        pri_ord,
        bak_ord,
        call_ref,
        mut dialog,
    } = place("limiter-release-at-an-unconfirmed-verdict").await;
    let (primary, backup) = if pri_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    fh.stall(&bak_ord, &pri_ord);
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(CONFIRMED_WITHIN).await;
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "the verdict is told");
    assert_eq!(backup.metrics().dead_verdicts_unconfirmed_total(), 1, "the stream speaks for it");
    let releases_before = rig.store.stats().releases_total;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [1, 1, 0], "the backup's end releases nothing");
    assert_eq!(rig.store.stats().releases_total, releases_before, "no release left");

    fh.resume(&bak_ord, &pri_ord);
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    let reconciled = fh
        .settle_terminal(async || {
            total_cdrs_for(&[primary], &call_ref) == 1
                && !primary.holds_any_trace(&call_ref).await
                && !backup.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(reconciled, "the primary reconciled the backup's end");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the primary released the holds");
    assert_eq!(rig.store.stats().releases_total, releases_before + 1, "once");
    rig.release_witnesses();
    assert_call_fully_over(&[primary, backup], &call_ref, &rig.store).await;
    drop(proxy);
}
