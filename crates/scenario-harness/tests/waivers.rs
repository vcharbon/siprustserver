//! Data-constructible scoped RFC-audit waivers: a `WaiverScope` waives exactly
//! one named violation on a chosen emitting party / message position, and a
//! declared waiver that filtered nothing is a loud error. SUT-less, through the
//! recording-wrapped simulated network. The deliberate violation exercised is a
//! CSeq reuse (RFC 3261 §12.2.1.1), driven via the declared CSeq deviation.

use scenario_harness::{Agent, CseqOp, CseqOpAt, CseqPattern, Harness, Proxy, WaiverScope};
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

/// A relayed alice→mid→bob call where alice commits a CSeq reuse. The reuse is
/// received at mid (emitted by alice) AND at bob (emitted by mid, which relayed
/// it) — two findings, attributed to their true emitters via `from_lane`.
async fn relay_call_with_alice_reuse(alice: &Agent, mid: &Proxy, bob: &Agent) {
    let bob_addr = bob.addr();
    let alice_addr = alice.addr();
    let mut call = alice.invite(bob).with_sdp(OFFER).through(mid.addr()).send().await;
    mid.forward_request(bob_addr).await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    mid.forward_response(alice_addr).await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    mid.forward_response(alice_addr).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    mid.forward_request(bob_addr).await;
    bob.receive("ACK").await;

    dialog.set_cseq_pattern(CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] });
    for _ in 0..2 {
        let mut i = dialog.send_request(InDialogMethod::Info).send().await;
        mid.forward_request(bob_addr).await;
        bob.receive("INFO").await.respond(200, "OK").await;
        mid.forward_response(alice_addr).await;
        i.expect(200).await;
    }
    let mut bye = dialog.bye().await;
    mid.forward_request(bob_addr).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    mid.forward_response(alice_addr).await;
    bye.expect(200).await;
    dialog.assert_deviations_consumed();
}

/// Relay lane: waiving BOTH the alice-emitted and the mid-relayed finding passes
/// — proving each is attributed to its TRUE emitter. (A real SUT lane would
/// never waive the SUT/mid side; here mid is a stand-in relayer.)
#[tokio::test]
async fn relay_findings_attributed_to_their_true_emitters() {
    let h = Harness::new("waiver-relay-both").describe("relay reuse: alice-emitted + mid-relayed, both waived");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let mid = h.proxy("mid", "127.0.0.1:5080").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // alice's copy (received at mid) and mid's relayed copy (received at bob).
    h.waive(WaiverScope::rule(CSEQ_RULE, "alice's replayed reuse").on_party("alice"));
    h.waive(WaiverScope::rule(CSEQ_RULE, "mid relayed it (a real SUT lane never waives the SUT)").on_party("mid"));
    relay_call_with_alice_reuse(&alice, &mid, &bob).await;
    h.finish().await;
}

/// Relay lane, the SUT-safety property: an alice-scoped waiver filters ONLY the
/// alice-emitted finding; the copy mid relayed (emitted by mid) still GATES — a
/// peer waiver never un-audits the relayer/SUT.
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn relay_peer_waiver_leaves_relayed_copy_gated() {
    let h = Harness::new("waiver-relay-alice").describe("alice-scoped waiver leaves the mid-relayed copy gated");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let mid = h.proxy("mid", "127.0.0.1:5080").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(WaiverScope::rule(CSEQ_RULE, "only alice's copy").on_party("alice"));
    relay_call_with_alice_reuse(&alice, &mid, &bob).await;
    h.finish().await; // the mid-relayed finding gates → panic
}

/// Relay lane, symmetric: a mid-scoped waiver leaves the alice-emitted finding
/// gated.
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn relay_mid_waiver_leaves_alice_finding_gated() {
    let h = Harness::new("waiver-relay-mid").describe("mid-scoped waiver leaves the alice-emitted finding gated");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let mid = h.proxy("mid", "127.0.0.1:5080").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    h.waive(WaiverScope::rule(CSEQ_RULE, "only mid's relayed copy").on_party("mid"));
    relay_call_with_alice_reuse(&alice, &mid, &bob).await;
    h.finish().await; // the alice-emitted finding gates → panic
}

/// at_position pins the OFFENDER: waiving the compliant INVITE's wire position
/// does NOT cover the INFO's reuse (which carries a different `offending` index).
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn position_waiver_pins_offender_not_a_compliant_neighbour() {
    let h = Harness::new("waiver-pos-offender").describe("waiving the INVITE's position does not cover the reuse");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    call_with_alice_reuse(&alice, &bob).await;
    let invite_pos = h
        .wire_entries()
        .iter()
        .position(|e| e.raw.starts_with(b"INVITE "))
        .map(|i| i + 1)
        .expect("an INVITE on the wire");
    h.waive(WaiverScope::rule(CSEQ_RULE, "waive the (compliant) INVITE").at_position(invite_pos));
    h.finish().await; // the reuse's offending position is not the INVITE's → gates
}

/// Two reuses in ONE dialog: a position-scoped waiver covers exactly its
/// offender; the second reuse still gates.
#[tokio::test]
#[should_panic(expected = "RFC audit violation")]
async fn two_reuses_one_dialog_position_scopes_exactly_one() {
    let h = Harness::new("waiver-two-reuse").describe("one position waiver covers one of two reuses in a dialog");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Reuse at step 1 (CSeq 2 twice) AND at step 3 (CSeq 3 twice).
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }, CseqOpAt { at: 3, op: CseqOp::Reuse }],
    });
    for _ in 0..4 {
        let mut i = dialog.send_request(InDialogMethod::Info).send().await;
        bob.receive("INFO").await.respond(200, "OK").await;
        i.expect(200).await;
    }
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    dialog.assert_deviations_consumed();

    // Waive only the FIRST reuse (the 2nd INFO on the wire); the second reuse
    // (the 4th INFO) still gates.
    let first_reuse_pos = info_positions(&h)[1];
    h.waive(WaiverScope::rule(CSEQ_RULE, "only the first reuse").at_position(first_reuse_pos));
    h.finish().await;
}
