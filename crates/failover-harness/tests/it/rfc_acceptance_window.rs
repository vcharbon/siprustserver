//! **The RFC-audit acceptance WINDOW** — `FailoverHarness::{accept_rfc_deviations_from_now,
//! resume_rfc_gate}`. A takeover-only deviation (ADR-0014's accepted dual-owner
//! in-dialog CSeq overlap) must be accepted *for the takeover window only*: the
//! same rule keeps gating on establishment and on everything after the window
//! closes, so a new regression outside it still fails the test.
//!
//! Peer-only scenarios (alice ⇄ bob, no worker, no proxy): the deviation is
//! alice's, declared through the `CseqPattern` DSL, so the SUT-side output is not
//! what is being excused. Each call is fully torn down (BYE + 200) before the
//! harness drops and runs its hard gate.

use std::time::Duration;

use failover_harness::{FailoverHarness, RULE_CSEQ_IN_DIALOG_ORDER};
use scenario_harness::{Agent, CseqOp, CseqOpAt, CseqPattern};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The acceptance the failover suites declare, worded as they word it.
const JUSTIFICATION: &str = "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq \
                             overlap in the takeover window — one call drops cleanly";

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

/// A deviation emitted INSIDE an open window is accepted: the Drop-time gate
/// passes, and the deviation is still reported (classified, not masked).
#[tokio::test(start_paused = true)]
async fn deviation_inside_the_window_is_accepted_and_recorded() {
    let mut fh = FailoverHarness::new("rfc-window-accepted", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    // The "fault instant": everything from here to the end of the run is accepted.
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, JUSTIFICATION);
    fh.advance(Duration::from_secs(1)).await;
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;

    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    let accepted = fh.accepted_rfc_deviations();
    assert_eq!(
        accepted.len(),
        1,
        "the window accepted exactly the in-window CSeq reuse (non-vacuity): {accepted:?}",
    );
    assert!(
        accepted[0].1.contains("CSeq"),
        "the accepted deviation is the CSeq reuse: {accepted:?}",
    );
}

/// The SAME deviation emitted BEFORE the window opens still gates — the case a
/// harness-lifetime waiver silently covered: establishment is audited in full.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "RFC 3261 audit violation")]
async fn deviation_before_the_window_opens_still_gates() {
    let mut fh = FailoverHarness::new("rfc-window-before", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;

    // Opened only now — after the offending message. It covers nothing, so the
    // Drop-time gate must still fail on the pre-window deviation.
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, JUSTIFICATION);
    assert!(
        fh.accepted_rfc_deviations().is_empty(),
        "a window opened after the offending message accepts nothing",
    );
}

/// A deviation emitted AFTER the window closes gates again: `resume_rfc_gate` puts
/// the rule back in force for the rest of the run.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "RFC 3261 audit violation")]
async fn deviation_after_the_window_closes_gates_again() {
    let mut fh = FailoverHarness::new("rfc-window-after", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, JUSTIFICATION);
    fh.advance(Duration::from_secs(1)).await;
    fh.resume_rfc_gate(RULE_CSEQ_IN_DIALOG_ORDER);
    fh.advance(Duration::from_secs(1)).await;

    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
}

/// A two-phase scenario arms the SAME rule twice (two faults); one
/// `resume_rfc_gate` must close BOTH — an older still-open window may not keep the
/// rule accepted for the rest of the run.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "RFC 3261 audit violation")]
async fn resume_closes_every_open_window_for_the_rule() {
    let mut fh = FailoverHarness::new("rfc-window-two-arms", &["b1"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let mut dialog = establish(&alice, &bob).await;
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, JUSTIFICATION);
    fh.advance(Duration::from_secs(1)).await;
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, JUSTIFICATION);
    fh.advance(Duration::from_secs(1)).await;
    fh.resume_rfc_gate(RULE_CSEQ_IN_DIALOG_ORDER);
    fh.advance(Duration::from_secs(1)).await;

    commit_cseq_reuse(&mut dialog, &alice, &bob).await;
    hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(1)).await;
}
