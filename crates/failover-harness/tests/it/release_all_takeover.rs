//! **A release-all reaching a takeover copy.** The copy a backup serves for a
//! dead primary is released as the backup's own calls are (ADR-0050 §10):
//! nothing is sent to either party. The copy defers the call's end to its
//! primary, still a member of the set (ADR-0020 X3), so the backup writes no
//! record yet; when the primary departs, the departure sweep discharges the
//! ended call (ADR-0047 D8): one CDR, closed under `operator_release`, and the
//! call's limiter holds released once.
//!
//! ```text
//!   established · P serves · B synchronized
//!   P crashes in place (its endpoint stays in the set)
//!   alice ──INFO──▶ proxy ──▶ B   (takeover copy, its transaction still open)
//!   operator ──release-all──▶ B   (copy ended, deferred to P)
//!   P's endpoint leaves the set   (B's sweep: CDR, release, delete)
//! ```
//!
//! The parties are left holding a dialog the SUT dropped: that is what the
//! test asserts. Each ends its side with a BYE, and the `481` terminates it
//! (RFC 3261 §12.2.1.2).

use std::time::Duration;

use b2bua::release_all::{ReleasedAll, REASON};
use call::TerminationCause;
use failover_harness::{assert_call_fully_over, cookie_field, FailoverHarness, WorkerHealth};
use sip_message::generators::InDialogMethod;

use crate::limiter_release_by_call::{limited_decision, spawn_workers, LimiterRig};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

#[tokio::test(start_paused = true)]
async fn a_released_takeover_copy_is_settled_once_by_the_departure_sweep() {
    let mut fh = FailoverHarness::new("release-all-takeover-copy", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rig = LimiterRig::serve().await;
    let (proxy, mut w_b1, mut w_b2) = spawn_workers(&mut fh, &rig, limited_decision(&["x"])).await;

    // ── established on the primary, replicated ──────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let pri = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(rig.holds(), [1, 0, 0], "the call holds x");
    let (primary, backup) =
        if pri == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let call_ref = backup.scan_one_backed_up(&pri).await.expect("the call replicated");

    // ── the primary crashes in place; an INFO makes the backup serve the call
    primary.crash();
    proxy.set_health(&pri, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    assert!(backup.serves(&call_ref), "the backup serves a takeover copy");

    // ── the operator releases the backup's calls ────────────────────────────
    let mark = fh.sip_entries().len();
    assert_eq!(backup.release_all(), ReleasedAll { released: 1, aborted: 0 });
    fh.advance(Duration::from_secs(1)).await;
    let parties = [alice.addr(), bob.addr()];
    let sent = fh.sip_entries().into_iter().skip(mark).filter(|e| parties.contains(&e.to)).count();
    assert_eq!(sent, 0, "nothing is sent to either party");
    assert!(!backup.serves(&call_ref), "the copy is released");
    assert!(backup.cdr_records().is_empty(), "the copy defers the call's end to its primary");
    assert_eq!(rig.holds(), [1, 0, 0], "and its release");

    // ── the primary departs: the sweep settles the call ─────────────────────
    backup.simulate_peer_removed(&pri);
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(rig.holds(), [0, 0, 0], "the sweep released the call's holds");
    let records = backup.cdr_records();
    let [record] = records.as_slice() else { panic!("one record: {records:#?}") };
    let cause = record.termination.as_ref().map(|t| t.cause);
    assert_eq!(cause, Some(TerminationCause::OperatorRelease));
    assert!(record.events.iter().any(|e| e.reason.as_deref() == Some(REASON)));

    dialog.bye().await.expect(481).await;
    bob_dialog.bye().await.expect(481).await;
    fh.advance(Duration::from_secs(1)).await;
    rig.release_witnesses();
    assert_call_fully_over(&[&*backup], &call_ref, &rig.store).await;
    drop(proxy);
}
