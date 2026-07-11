//! §17.1.1.3 UAS-side ACK obligation (newkahneed-036 ask B) — the server
//! transaction OWNS the hop ACK to a non-2xx INVITE final. Matching is keyed
//! `(Call-ID, INVITE top-Via branch)`, never positional, so the ACK may land
//! **before or after** the body's next receive (the reroute interleave) with
//! an identical body; `expect_ack` asserts it at a chosen point; and a body
//! that never consumes it at all still passes `finish()` because the arrival
//! is recorded at delivery (036 ask A) and discharges the gating
//! `rfc3261.unackedInviteNon2xxFinal` wire rule.

use scenario_harness::Harness;

/// The body carries NO ACK-receive after rejecting: the txn layer + the
/// delivery-time recording settle the obligation at `finish()`. This is the
/// acceptance shape for the nk_reroute workaround deletion.
#[tokio::test]
async fn rejected_invite_needs_no_ack_boilerplate() {
    let h = Harness::new("reject-no-ack-boilerplate").describe(
        "bob rejects with 486 and never reads the hop ACK; the recorded \
         arrival discharges the §17.1.1.3 audit obligation at finish()",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    // The client transaction auto-ACKs the surfaced non-2xx final (034 ask B).
    call.expect(486).await;
    // bob NEVER touches its socket again — the ACK sits unconsumed.

    let report = h.finish().await;
    let ack = report
        .entries()
        .iter()
        .find(|e| e.raw.starts_with(b"ACK "))
        .expect("the hop ACK must be on the recorded trace")
        .clone();
    assert!(ack.delivered);
    assert_eq!(
        ack.recv_note,
        Some(sip_net::RecvNote::Unconsumed),
        "nothing read it — the ladder says so"
    );
}

/// `expect_ack` as the explicit point-in-flow assertion, in BOTH interleave
/// orders relative to the next transaction's INVITE:
/// - ACK first: bob's `receive("INVITE")` absorbs the txn-owned ACK instead of
///   erroring "expected INVITE, got ACK";
/// - ACK last: bob receives the new INVITE before the reject's ACK exists at
///   all, then `expect_ack` pulls it.
#[tokio::test]
async fn ack_races_next_invite_in_either_order() {
    let h = Harness::new("ack-races-next-invite");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // ── Order 1: ACK arrives BEFORE the next INVITE is received ────────────
    let mut call1 = alice.invite(&bob).send().await;
    let mut uas1 = bob.receive("INVITE").await;
    let call1_id = uas1.request().call_id.clone();
    uas1.respond(486, "Busy Here").await;
    call1.expect(486).await; // auto-ACK goes on the wire now

    let mut call2 = alice.invite(&bob).send().await;
    // bob's queue: [ACK(call1), INVITE(call2)] — the txn-owned ACK is absorbed.
    let mut uas2 = bob.receive("INVITE").await;
    assert_ne!(uas2.request().call_id, call1_id, "the surfaced request is the NEW invite");
    // The obligation was already claimed in passing: this returns immediately.
    uas1.expect_ack().await;

    // ── Order 2: the next INVITE is received BEFORE the ACK exists ─────────
    uas2.respond(486, "Busy Here").await;
    let mut call3 = alice.invite(&bob).send().await;
    // call2's 486 is still unread at alice, so its auto-ACK has not been sent:
    // bob sees the new INVITE first.
    let uas3 = bob.receive("INVITE").await;
    assert_ne!(uas3.request().call_id, uas2.request().call_id);
    call2.expect(486).await; // now the ACK for call2's reject goes out
    uas2.expect_ack().await; // pulled (or already sighted) — keyed, not positional

    // Terminate the surviving call properly: 200 → ACK → BYE → 200.
    let mut uas3 = uas3;
    uas3.respond(200, "OK").await;
    call3.expect(200).await;
    let mut dialog = call3.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    h.finish().await;
}

/// Negative control: a peer that genuinely never ACKs. `try_expect_ack` times
/// out (the active assertion), and the wire rule still finds the violation —
/// the finding tracks the wire, not the body's receive calls — so the test
/// must waive it explicitly (the buggy side is the PEER, alice).
#[tokio::test]
async fn missing_ack_times_out_and_gates() {
    let h = Harness::new("missing-ack-negative-control");
    // Deliberate peer bug under test: alice never ACKs bob's 486. The waiver
    // sanctions the peer-side violation; bob's own output stays compliant.
    h.allow_violation(
        "rfc3261.unackedInviteNon2xxFinal",
        "the test's PURPOSE is a peer that never ACKs a non-2xx final",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // Hand-rolled client side: send the INVITE but never surface the final —
    // the §17.1.1.3 auto-ACK never runs, so no ACK ever exists on the wire.
    let _call = alice.invite(&bob).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(486, "Busy Here").await;

    let err = uas.try_expect_ack().await.expect_err("no ACK can arrive");
    assert!(
        matches!(err, scenario_harness::StepError::Timeout { .. }),
        "expect_ack fails actively at the point of the flow: {err}"
    );

    h.finish().await;
}
