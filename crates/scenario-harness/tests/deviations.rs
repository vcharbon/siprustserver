//! Declared deviations on the dialog handle: the CSeq relative-pattern
//! (offset/jump/reuse) and the `delayed-automatic` ACK. SUT-less, through the
//! recording-wrapped simulated network.

use std::time::Duration;

use scenario_harness::{
    CseqOp, CseqOpAt, CseqPattern, EmitOpts, Harness, MessageTemplate, TemplateHeader, WaiverScope,
};
use sip_message::generators::InDialogMethod;
use sip_message::Method;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// A declared CSeq JUMP is visible on the wire: the deviated in-dialog request
/// carries base+jump, and subsequent requests continue from the jumped value.
/// The out-of-pattern CSeq leaves a §12.2.1.1 gap the audit flags — sanctioned
/// via `allow_violation` (alice is the peer replaying a captured anomaly).
#[tokio::test]
async fn cseq_jump_visible_on_the_wire() {
    let h = Harness::new("cseq-jump").describe(
        "A declared CSeq jump at step 0 emits base+jump on the wire; the next \
         in-dialog request continues from there",
    );
    // Replaying a captured out-of-pattern in-dialog CSeq (a jump) — scoped to
    // ALICE's emissions (the peer replaying the anomaly).
    h.waive(
        WaiverScope::rule(
            "rfc3261.cseqInDialogOrder",
            "replaying a captured out-of-pattern in-dialog CSeq (declared jump deviation)",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Establish (INVITE CSeq 1).
    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Declare a jump by +48 at the first in-dialog request.
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: 48 } }],
    });

    // Step 0: UPDATE → base 2 + 48 = 50.
    let mut upd = dialog.send_request(InDialogMethod::Update).with_sdp(OFFER).send().await;
    let mut ubob = bob.receive("UPDATE").await;
    assert_eq!(ubob.request().cseq.seq, 50, "UPDATE carries base+jump on the wire");
    ubob.respond(200, "OK").with_sdp(ANSWER).await;
    upd.expect(200).await;

    // Step 1: BYE → continues from the jump = 51.
    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    assert_eq!(bbob.request().cseq.seq, 51, "the next request continues from the jump");
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    dialog.assert_deviations_consumed();
    h.finish().await;
}

/// A declared CSeq REUSE emits the same number twice. This is deliberately
/// non-compliant peer behavior; the §12.2.1.1 audit fires on the reuse, so it is
/// sanctioned via `allow_violation` with the deviation as justification.
#[tokio::test]
async fn cseq_reuse_emitted_as_declared() {
    let h = Harness::new("cseq-reuse").describe(
        "A declared CSeq reuse emits the previous number again; the audit fires \
         on the reuse and is waived with the deviation as justification",
    );
    h.waive(
        WaiverScope::rule(
            "rfc3261.cseqInDialogOrder",
            "replaying a captured in-dialog CSeq reuse (declared reuse deviation)",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Reuse at step 1: the second in-dialog request repeats step 0's number.
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }],
    });

    // Step 0: INFO CSeq 2.
    let mut i0 = dialog.send_request(InDialogMethod::Info).send().await;
    let mut r0 = bob.receive("INFO").await;
    assert_eq!(r0.request().cseq.seq, 2);
    r0.respond(200, "OK").await;
    i0.expect(200).await;

    // Step 1: INFO reuses CSeq 2.
    let mut i1 = dialog.send_request(InDialogMethod::Info).send().await;
    let mut r1 = bob.receive("INFO").await;
    assert_eq!(r1.request().cseq.seq, 2, "reuse emits the previous number");
    r1.respond(200, "OK").await;
    i1.expect(200).await;

    // Step 2: BYE continues from the reused number = 3.
    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    assert_eq!(bbob.request().cseq.seq, 3, "continues from the reused number");
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    dialog.assert_deviations_consumed();
    h.finish().await;
}

/// A dialog with NO declared deviation keeps the stack's base numbering
/// (byte-identical to a dialog without a pattern); no waiver needed (compliant).
#[tokio::test]
async fn zero_deviation_cseq_is_natural() {
    let h = Harness::new("cseq-natural").describe(
        "An undeclared dialog uses the stack's base CSeq numbering (2, 3, …); \
         no deviation, RFC-compliant, audit green",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // No pattern declared.
    let mut upd = dialog.send_request(InDialogMethod::Update).with_sdp(OFFER).send().await;
    let mut ubob = bob.receive("UPDATE").await;
    assert_eq!(ubob.request().cseq.seq, 2, "natural increment");
    ubob.respond(200, "OK").with_sdp(ANSWER).await;
    upd.expect(200).await;

    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    assert_eq!(bbob.request().cseq.seq, 3, "natural increment continues");
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// ADR-0024 §6: the CSeq step counter is SHARED across a scope-refresh clone —
/// the exact `dialog.clone()` the actor engine takes into its teardown scope.
/// Consuming a deviation op on the LIVE dialog advances the ONE shared counter
/// for the clone too, so a teardown BYE never re-consumes an op the live dialog
/// already did. A FORKED counter (the pre-§6 hazard) would leave the clone's op
/// pending, and `assert_deviations_consumed` on the clone would panic.
#[tokio::test]
async fn shared_cseq_counter_survives_a_scope_refresh_clone() {
    let h = Harness::new("cseq-shared-clone").describe(
        "consuming a CSeq-deviation op on the live dialog advances the SHARED \
         counter for a scope-refresh clone too — no fork, no re-consumed op",
    );
    h.waive(
        WaiverScope::rule("rfc3261.cseqInDialogOrder", "declared jump; shared-counter clone test")
            .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // One op at step 0, then a scope-refresh clone taken BEFORE it is consumed —
    // the exact shape of the actor engine's `set_confirmed(dialog.clone())`.
    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: 48 } }],
    });
    let clone = dialog.clone();

    // Consume step 0 on the LIVE dialog (OPTIONS → base 2 + jump 48 = 50).
    let mut opt = dialog.send_request(InDialogMethod::Options).send().await;
    let mut obob = bob.receive("OPTIONS").await;
    assert_eq!(obob.request().cseq.seq, 50, "the jump is applied on the live dialog");
    obob.respond(200, "OK").await;
    opt.expect(200).await;

    // The clone SHARES the counter, so it now sees the op consumed — a forked
    // counter would still have step 0 pending here and this would panic.
    clone.assert_deviations_consumed();

    // Clean teardown from the live dialog; the BYE continues the shared counter.
    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    assert_eq!(bbob.request().cseq.seq, 51, "the BYE continues from the jump (shared counter)");
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// The `delayed-automatic` ACK: alice holds the ACK to the 2xx for ~2.8 s
/// (paused clock). bob retransmits the 200 meanwhile (RFC 3261 §13.3.1.4); the
/// late ACK lands, retransmissions stop, the call terminates clean and the audit
/// is green. Drives virtual time via `tokio::time` under a PAUSED runtime — no
/// wall-clock sleeps (see docs/testing/test-clock.md).
#[tokio::test(start_paused = true)]
async fn delayed_automatic_ack_provokes_retx_and_settles() {
    let h = Harness::new("delayed-ack").describe(
        "alice holds the automatic ACK ~2.8s; bob retransmits the 200 meanwhile; \
         the late ACK settles the call clean (the eCall-OK shape)",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .delayed_ack(Duration::from_millis(2800))
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;

    // alice receives the 200 and (per the deviation) holds the ACK 2.8s; bob
    // retransmits the 200 every second until the ACK lands. Concurrent futures on
    // ONE paused-clock task (no wall time); tokio auto-advances to each timer.
    let alice_side = async {
        call.expect(200).await;
        call.ack_delayed().await
    };
    let bob_side = async {
        let mut retx = 0u32;
        loop {
            tokio::select! {
                biased;
                _ack = bob.receive("ACK") => break retx,
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                    // Cap the loop so a future refactor that never lands the ACK
                    // fails loudly instead of hanging.
                    assert!(retx < 8, "bob retransmitted the 200 {retx}× with no ACK — the held ACK never landed");
                    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
                    retx += 1;
                }
            }
        }
    };
    let (mut dialog, retx) = tokio::join!(alice_side, bob_side);
    assert!(retx >= 1, "the peer retransmitted the 200 at least once while the ACK was held (got {retx})");

    // Drain any 200 retransmits still queued at alice's socket (recorded received).
    alice.drain().await;

    // Tear down clean.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// A declared `delayed-automatic` binds a PLAIN `ack()` too: the declaration
/// owns the automatic, so `ack()` (not just `ack_delayed()`) holds the ACK —
/// proven by the peer retransmitting the 200 before the ACK lands.
#[tokio::test(start_paused = true)]
async fn declared_delay_binds_plain_ack() {
    let h = Harness::new("delayed-ack-plain").describe(
        "a declared delayed_ack + a plain ack() still holds the ACK (the \
         declaration owns the automatic); the peer retransmits the 200",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .delayed_ack(Duration::from_millis(1500))
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;

    let alice_side = async {
        call.expect(200).await;
        call.ack().await // PLAIN ack() — must honour the declaration
    };
    let bob_side = async {
        let mut retx = 0u32;
        loop {
            tokio::select! {
                biased;
                _ack = bob.receive("ACK") => break retx,
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {
                    assert!(retx < 8, "held ACK never landed (got {retx} retx)");
                    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
                    retx += 1;
                }
            }
        }
    };
    let (mut dialog, retx) = tokio::join!(alice_side, bob_side);
    assert!(retx >= 1, "plain ack() honoured the declared delay (peer retransmitted; got {retx})");

    alice.drain().await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    h.finish().await;
}

/// A TEMPLATED in-dialog request honors the declared CSeq pattern: a templated
/// INFO carries base+jump, and a subsequent BYE continues from there. (The
/// out-of-pattern CSeq trips the §12.2.1.1 audit — waived as the replayed
/// peer anomaly.)
#[tokio::test]
async fn templated_in_dialog_request_honors_the_pattern() {
    let h = Harness::new("template-plus-jump").describe(
        "a templated INFO honors the declared CSeq jump (base+jump), BYE continues",
    );
    h.waive(
        WaiverScope::rule(
            "rfc3261.cseqInDialogOrder",
            "replaying a captured out-of-pattern CSeq via a templated in-dialog request",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    dialog.set_cseq_pattern(CseqPattern {
        offset: 0,
        ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: 48 } }],
    });

    // Step 0: a TEMPLATED INFO → base 2 + 48 = 50.
    let info = MessageTemplate::request(
        Method::Info,
        vec![TemplateHeader::frozen("Content-Type", "application/xml")],
        b"<info/>".to_vec(),
    );
    let mut i = dialog.send_template(&info, EmitOpts::default()).await;
    let mut r = bob.receive("INFO").await;
    assert_eq!(r.request().cseq.seq, 50, "the templated INFO honours the jump");
    assert_eq!(
        sip_message::message_helpers::get_header(&r.request().headers, "content-type"),
        Some("application/xml"),
        "the template's frozen header still rides",
    );
    r.respond(200, "OK").await;
    i.expect(200).await;

    // Step 1: BYE continues from the jump = 51.
    let mut bye = dialog.bye().await;
    let mut bbob = bob.receive("BYE").await;
    assert_eq!(bbob.request().cseq.seq, 51, "the next request continues from the jump");
    bbob.respond(200, "OK").await;
    bye.expect(200).await;

    dialog.assert_deviations_consumed();
    h.finish().await;
}
