//! Data-constructible scoped RFC-audit waivers: a `WaiverScope` waives exactly
//! one named violation on a chosen emitting party / message position, and a
//! declared waiver that filtered nothing is a loud error. SUT-less, through the
//! recording-wrapped simulated network. The deliberate violation exercised is a
//! CSeq reuse (RFC 3261 §12.2.1.1), driven via the declared CSeq deviation.

use scenario_harness::{Agent, CseqOp, CseqOpAt, CseqPattern, Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

const CSEQ_RULE: &str = "rfc3261.cseqInDialogOrder";

/// A complete alice→bob call in which alice commits a CSeq reuse (two INFOs on
/// the same number) via the declared deviation, then tears down. Does NOT call
/// `finish` — the test owns the gate.
async fn call_with_alice_reuse(alice: &Agent, bob: &Agent) {
    let mut call = alice.invite(bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    dialog.set_cseq_pattern(CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] });
    let mut i0 = dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    i0.expect(200).await;
    let mut i1 = dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    i1.expect(200).await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    dialog.assert_deviations_consumed();
}

/// 1-based wire positions of every INFO request in the recording so far.
fn info_positions(h: &Harness) -> Vec<usize> {
    h.wire_entries()
        .iter()
        .enumerate()
        .filter(|(_, e)| e.raw.starts_with(b"INFO "))
        .map(|(i, _)| i + 1)
        .collect()
}

/// A party-scoped waiver over exactly the offending party passes the gate.
#[tokio::test]
async fn scoped_waiver_passes() {
    let h = Harness::new("waiver-pass").describe("alice's cseq reuse waived by an alice-scoped waiver");
    h.waive(WaiverScope::rule(CSEQ_RULE, "replayed peer CSeq reuse").on_party("alice"));
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await;
    h.finish().await;
}

/// The SAME violation with the waiver ABSENT fails at finish().
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn unwaived_violation_fails() {
    let h = Harness::new("waiver-absent").describe("the same cseq reuse with no waiver must fail the gate");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await;
    h.finish().await;
}

/// The coarse `allow_violation` still waives (any party, any message).
#[tokio::test]
async fn coarse_allow_violation_still_waives() {
    let h = Harness::new("waiver-coarse").describe("allow_violation waives the reuse regardless of party");
    h.allow_violation(CSEQ_RULE, "replayed peer CSeq reuse (coarse)");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await;
    h.finish().await;
}

/// Side scoping is a MECHANISM: an alice-scoped waiver does NOT filter the same
/// rule attributed to bob. alice AND bob each commit the reuse; only alice's is
/// waived, so finish() fails on bob's.
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn peer_scoped_waiver_does_not_filter_other_party() {
    let h = Harness::new("waiver-side").describe("alice-scoped waiver leaves bob's identical violation gated");
    h.waive(WaiverScope::rule(CSEQ_RULE, "replayed alice CSeq reuse").on_party("alice"));
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    // alice's reuse (waived).
    alice_dialog.set_cseq_pattern(CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] });
    let mut ai0 = alice_dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    ai0.expect(200).await;
    let mut ai1 = alice_dialog.send_request(InDialogMethod::Info).send().await;
    bob.receive("INFO").await.respond(200, "OK").await;
    ai1.expect(200).await;

    // bob's reuse of the SAME class (NOT waived — different emitting party).
    bob_dialog.set_cseq_pattern(CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] });
    let mut bi0 = bob_dialog.send_request(InDialogMethod::Info).send().await;
    alice.receive("INFO").await.respond(200, "OK").await;
    bi0.expect(200).await;
    let mut bi1 = bob_dialog.send_request(InDialogMethod::Info).send().await;
    alice.receive("INFO").await.respond(200, "OK").await;
    bi1.expect(200).await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await; // alice waived; bob's gates → panic
}

/// A position-scoped waiver covers its own occurrence — passes.
#[tokio::test]
async fn message_position_scoping_covers_its_occurrence() {
    let h = Harness::new("waiver-pos-ok").describe("a waiver at the reuse's wire position covers it");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await;

    // The reuse is the 2nd INFO; scope the waiver to its wire position.
    let reuse_pos = info_positions(&h)[1];
    h.waive(WaiverScope::rule(CSEQ_RULE, "replayed reuse at this position").at_position(reuse_pos));
    h.finish().await;
}

/// A waiver scoped to ONE message position does not cover a second occurrence
/// elsewhere: two calls each reuse, waive only the first's position → finish
/// fails on the second.
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn message_position_scoping_does_not_cover_second() {
    let h = Harness::new("waiver-pos-scope").describe("a one-position waiver leaves a second occurrence gated");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await; // call 1
    call_with_alice_reuse(&alice, &bob).await; // call 2

    // Waive only call 1's reuse (its INFOs are the first two INFOs on the wire).
    let call1_reuse_pos = info_positions(&h)[1];
    h.waive(WaiverScope::rule(CSEQ_RULE, "waive only call 1's reuse").at_position(call1_reuse_pos));
    h.finish().await; // call 2's reuse is not covered → panic
}

/// A declared waiver that filtered nothing is a loud error at finish().
#[tokio::test]
#[should_panic(expected = "matched no finding")]
async fn unused_waiver_is_a_loud_error() {
    let h = Harness::new("waiver-unused").describe("a dead waiver is an error");
    // A clean call — nothing to waive.
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(WaiverScope::rule(CSEQ_RULE, "this never fires").on_party("alice"));
    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await; // the unused waiver → panic
}

/// A conditional waiver opts out of the unused-waiver error — passes even when
/// it filters nothing.
#[tokio::test]
async fn unused_waiver_conditional_opts_out() {
    let h = Harness::new("waiver-conditional").describe("a conditional dead waiver passes");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(WaiverScope::rule(CSEQ_RULE, "may or may not fire").on_party("alice").conditional());
    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await; // conditional → no unused-waiver error
}
