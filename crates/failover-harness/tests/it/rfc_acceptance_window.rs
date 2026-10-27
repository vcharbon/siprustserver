//! **The windows a strike opens** — `FailoverHarness::strike`. A deviation a
//! row of the window table declares (ADR-0055 §11, such as the CSeq one copy of
//! a split re-uses after the other sent it, §8) is accepted from the strike on
//! and named by its row: the same rule keeps gating on establishment, and a
//! row the run's measured tier keeps closed accepts nothing.
//!
//! Peer-only scenarios (alice ⇄ bob, no worker, no proxy): the deviation is
//! alice's, declared through the `CseqPattern` DSL, so the SUT-side output is not
//! what is being excused. Each call is fully torn down (BYE + 200) before the
//! harness drops and runs its hard gate.

use std::time::Duration;

use b2bua::repl::SyncLevel;
use failover_harness::{FailoverHarness, Incident, Tier};
use scenario_harness::{Agent, CseqOp, CseqOpAt, CseqPattern};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// Establish alice ⇄ bob (INVITE / 180 / 200 / ACK) and hand back alice's dialog.
async fn establish(alice: &Agent, bob: &Agent) -> scenario_harness::Dialog {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    dialog
}

/// Two INFOs on the SAME in-dialog CSeq — the §12.2.1.1 reuse the audit flags,
/// emitted by alice at the current clock reading. The OFFENDING message is the
/// second INFO, so where the harness clock stands when *it* is sent decides whether
/// an acceptance window covers the finding.
async fn commit_cseq_reuse(dialog: &mut scenario_harness::Dialog, alice: &Agent, bob: &Agent) {
    let _ = alice;
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }],
    });
    let mut first = dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    first.expect(200).await;
    let mut reused = dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    reused.expect(200).await;
    dialog.assert_deviations_consumed();
}

/// Terminate the dialog so the scenario ends with the call properly over.
async fn hangup(dialog: &mut scenario_harness::Dialog, bob: &Agent) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
}

/// A deviation emitted after a strike that opens its row is accepted: the
/// Drop-time gate passes, and the deviation is still reported (classified,
/// not masked) with the row that took it.
#[tokio::test(start_paused = true)]
async fn deviation_after_the_strike_is_accepted_and_named_by_its_row() {
    let mut fh = FailoverHarness::new("rfc-window-accepted", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    fh.strike(Incident::Split);
    fh.advance(Duration::from_secs(1)).await;
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;

    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    let accepted = fh.accepted_rfc_deviations();
    assert_eq!(accepted.len(), 1, "the strike accepted exactly the CSeq reuse: {accepted:?}");
    let ((_, _, detail), row) = &accepted[0];
    assert!(detail.contains("CSeq"), "the accepted deviation is the CSeq reuse: {detail}");
    assert_eq!(*row, "W-split-cseq", "a split's CSeq re-use is the W-split-cseq row's");
    assert_eq!(fh.tier(), Some(Tier::Degraded), "a split is never Guaranteed");
}

/// The SAME deviation emitted BEFORE the strike still gates: establishment
/// is audited in full.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "RFC 3261 audit violation")]
async fn deviation_before_the_strike_still_gates() {
    let mut fh = FailoverHarness::new("rfc-window-before", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    // Struck only now — after the offending message. It covers nothing, so the
    // Drop-time gate must still fail on the earlier deviation.
    fh.strike(Incident::Split);
    assert!(
        fh.accepted_rfc_deviations().is_empty(),
        "a strike after the offending message accepts nothing",
    );
}

/// A kill of a call whose every turn was backed, with no cap hit, stands in
/// the Guaranteed tier: the row the kill opened fires there and fails the
/// run.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "W-kill-cseq (closed in the guaranteed tier)")]
async fn a_row_that_fires_in_the_guaranteed_tier_fails_the_run() {
    let mut fh = FailoverHarness::new("rfc-window-guaranteed", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.strike(Incident::PrimaryKilled);
    assert_eq!(fh.tier(), Some(Tier::Guaranteed));
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
}

/// The same kill at sync level `off` stands in the Off tier, where the row
/// is open.
#[tokio::test(start_paused = true)]
async fn the_same_row_is_open_at_level_off() {
    let mut fh = FailoverHarness::new("rfc-window-off", &["b1"]).with_sync_level(SyncLevel::Off);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.strike(Incident::PrimaryKilled);
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(fh.tier(), Some(Tier::Off));
    let rows: Vec<_> = fh.fired_rows().iter().map(|f| f.row.id).collect();
    assert_eq!(rows, ["W-kill-cseq"], "the kill's once-per-dialog CSeq re-use");
}
