//! **A new call whose primary dies before the call's first turn.** The
//! primary's router has not taken the INVITE's turn: no call exists, no
//! creation write reached the backup, and the caller holds no 100 (ADR-0070).
//! The primary dies; the proxy reads it dead and sends each of the caller's
//! retransmissions to the cookie's backup, as its retransmission memo places
//! them. The backup holds no record of the call: each copy is another
//! primary's INVITE, dropped unanswered with its transaction relinquished,
//! never a second call. Its router lags past the §17.2.1 bound as the first
//! copy arrives, and still no 100 leaves. The caller hears nothing until its
//! Timer B fires, and the previous hop's own timers fail the call over.
//!
//! Beyond the failover sweep: HA axis 1 of the 100's departure, with a lagging
//! backup router.

use std::time::Duration;

use b2bua_harness::Pace;
use failover_harness::{FailoverHarness, WorkerHealth};
use scenario_harness::callflow::OFFER_SDP;

use crate::departure_support::call_id_placed_on;
use crate::inflight_resend::bring_up;

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The call's primary, the worker its Call-ID is placed on, and its backup.
const PRIMARY: &str = "b1";
const BACKUP: &str = "b2";
/// RFC 3261 T1: the caller's first INVITE retransmission is due this long
/// after the INVITE, each later one twice the previous gap (Timer A).
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 Timer B (64·T1): the caller's INVITE transaction times out.
const TIMER_B: Duration = Duration::from_millis(32_000);
/// When the primary dies, after the INVITE's send: before the first
/// retransmission.
const DIES_AT: Duration = Duration::from_millis(300);
/// How long the backup's router lags after the first copy reached it: past
/// the §17.2.1 bound.
const BACKUP_LAG: Duration = Duration::from_millis(400);

#[tokio::test(start_paused = true)]
async fn the_copies_of_a_call_its_primary_never_started_reach_the_backup_and_draw_nothing() {
    let name = "primary-dies-before-the-calls-turn";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, w_b2) = bring_up(&mut fh).await;
    let pace = fh.turn_points().router_pace().clone();

    // ── both routers wedged: the primary never takes the INVITE's turn ──────
    fh.mark(PRIMARY, None, "stall", "both routers take no turn");
    pace.set(PRIMARY, Pace::Frozen);
    pace.set(BACKUP, Pace::Frozen);
    let call = alice
        .invite(&bob)
        .with_sdp(OFFER_SDP)
        .with_call_id(call_id_placed_on(PRIMARY, "dies-before-the-turn"))
        .through(proxy.addr())
        .send()
        .await;
    sip_clock::testkit::advance_settled(DIES_AT).await;
    assert_eq!(heard_by(&fh, &alice), 0, "no 100 from a primary whose router took no turn");

    fh.mark(PRIMARY, None, "crash", "before the call's first turn");
    w_b1.crash();
    proxy.set_health(PRIMARY, WorkerHealth::Dead);

    // ── the first copy reaches the backup, whose router lags ────────────────
    sip_clock::testkit::advance_settled(T1 - DIES_AT).await;
    call.retransmit().await;
    sip_clock::testkit::advance_settled(BACKUP_LAG).await;
    assert_eq!(copies_to(&fh, &w_b2), 1, "the copy goes to the cookie's backup");
    assert_eq!(heard_by(&fh, &alice), 0, "no 100 from a backup whose router lags");
    pace.set(BACKUP, Pace::Free);

    // ── every later copy is dropped as foreign, up to Timer B ───────────────
    let (mut now, mut last, mut gap, mut copies) = (T1 + BACKUP_LAG, T1, T1 * 2, 1);
    while last + gap < TIMER_B {
        last += gap;
        sip_clock::testkit::advance_settled(last - now).await;
        call.retransmit().await;
        (now, gap, copies) = (last, gap * 2, copies + 1);
    }
    sip_clock::testkit::advance_settled(TIMER_B - now).await;
    assert_eq!(copies, 6, "Timer A: copies at 0.5, 1.5, 3.5, 7.5, 15.5 and 31.5 s");
    assert_eq!(copies_to(&fh, &w_b2), copies, "every copy goes to the backup");
    assert_eq!(
        w_b2.metrics().foreign_invite_copies_dropped_total(),
        copies as u64,
        "each copy dropped as another primary's INVITE"
    );
    assert_eq!(heard_by(&fh, &alice), 0, "the caller hears nothing up to Timer B");
    assert!(!fh.sip_entries().iter().any(|e| e.to == bob.addr()), "no callee dialled");

    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert!(w_b2.memory_clean(), "the backup holds nothing of the call");
    assert!(w_b2.cdr_records().is_empty(), "no call, no CDR");
    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// The responses `agent` received, from the recording.
fn heard_by(fh: &FailoverHarness, agent: &scenario_harness::Agent) -> usize {
    let to_agent = |e: &&sip_net_testkit::RecordedSipEntry| e.to == agent.addr();
    fh.sip_entries().iter().filter(to_agent).filter(|e| e.raw.starts_with(b"SIP/2.0 ")).count()
}

/// The INVITE datagrams the proxy sent `node`, from the recording.
fn copies_to(fh: &FailoverHarness, node: &failover_harness::ReplicatedB2buaSut) -> usize {
    let to_node = |e: &&sip_net_testkit::RecordedSipEntry| e.to == node.sip_addr();
    fh.sip_entries().iter().filter(to_node).filter(|e| e.raw.starts_with(b"INVITE ")).count()
}
