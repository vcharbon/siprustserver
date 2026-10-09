//! **A BYE on an early dialog this stack already ended draws 481** (RFC 3261
//! §12.3, §12.2.2). A non-2xx final to the caller's INVITE ends every early
//! dialog its tagged provisionals opened, so a caller BYE that reaches this
//! stack after that final names no dialog. The answer is 481, whether the call
//! is still tearing down or already released, and the call's record and
//! cleanup are those of the release that came first.
//!
//! The shapes:
//! - the caller's BYE crosses the 487 that answers her own CANCEL;
//! - CANCEL and BYE leave together and the CANCEL is processed first;
//! - the callee's rejection crosses the caller's BYE and the call is released
//!   before the BYE arrives;
//! - the guard: a caller BYE crossing this stack's own BYE on a confirmed
//!   dialog still draws 200 (§15.1.2), since that dialog existed when it left.
//!
//! A BYE on a still-early dialog (200 + 487, the callee CANCELled) is
//! `early_dialog_bye.rs`.

use std::time::Duration;

use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use call::TerminationCause;
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Start lines of every datagram `agent` received, in arrival order.
fn start_lines(agent: &Agent) -> Vec<String> {
    agent.wire_view().iter().map(|e| e.start_line()).collect()
}

async fn reaped(b2bua: &B2buaSut) {
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The caller CANCELs a ringing call and BYEs the early dialog 50 ms later,
/// before the 487 reaches her. The stack has answered the CANCEL (200 + 487)
/// when the BYE arrives, so the BYE names the dialog that 487 ended: 481. The
/// callee's CANCEL completes as usual and the record is the caller's CANCEL.
///
/// Ordering this test depends on (100 ms transit per hop): the CANCEL leaves
/// at t and is answered at t+100; the BYE leaves at t+50 and arrives at
/// t+150; the 487 reaches the caller at t+200, after she sent her BYE.
#[tokio::test(start_paused = true)]
async fn caller_bye_crossing_the_487_to_her_cancel_draws_481() {
    let h = Harness::with_transit_delay("b2bua-ended-early-dialog-bye-after-487", 100);
    let alice = h.agent("alice", "127.0.0.1:5161").await;
    let bob = h.agent("bob", "127.0.0.1:5171").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5171).start(&h, "b2bua", "127.0.0.1:5181").await;
    let alice_addr = alice.addr();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    h.advance(Duration::from_millis(50)).await;
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    cxl.expect(200).await;
    call.expect(487).await; // auto-ACKed (§17.1.1.3)
    bye.expect(481).await;

    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteCancel, "the caller's CANCEL ended the call");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice_addr), [487], "one INVITE final");
}

/// CANCEL and BYE leave the caller in the same instant, the CANCEL first; the
/// stack processes them in that order, so the BYE arrives after the 487 that
/// ended its early dialog: 481, and no second final on the INVITE.
#[tokio::test(start_paused = true)]
async fn cancel_processed_before_a_simultaneous_bye_leaves_the_bye_481() {
    let h = Harness::with_transit_delay("b2bua-ended-early-dialog-cancel-bye-race", 1);
    let alice = h.agent("alice", "127.0.0.1:5162").await;
    let bob = h.agent("bob", "127.0.0.1:5172").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5172).start(&h, "b2bua", "127.0.0.1:5182").await;
    let alice_addr = alice.addr();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    cxl.expect(200).await;
    call.expect(487).await;
    bye.expect(481).await;

    let mut b_cxl = bob.receive("CANCEL").await;
    b_cxl.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteCancel);
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice_addr), [487], "one INVITE final");
}

/// The callee rejects the ringing call while the caller's BYE is in flight.
/// The rejection reaches the caller as the INVITE's final and the call is
/// released before her BYE arrives; the BYE resolves to no call and draws 481.
///
/// Ordering this test depends on (100 ms transit per hop): bob's 486 leaves at
/// t and is relayed at t+100; alice's BYE leaves at t+50 and arrives at t+150;
/// the 486 reaches her at t+200, after she sent her BYE.
#[tokio::test(start_paused = true)]
async fn caller_bye_after_the_rejected_call_is_released_draws_481() {
    let h = Harness::with_transit_delay("b2bua-ended-early-dialog-bye-after-release", 100);
    let alice = h.agent("alice", "127.0.0.1:5163").await;
    let bob = h.agent("bob", "127.0.0.1:5173").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5173).start(&h, "b2bua", "127.0.0.1:5183").await;
    let alice_addr = alice.addr();

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    b_inv.respond(486, "Busy Here").await;
    h.advance(Duration::from_millis(50)).await;
    let mut bye = call.send_request(InDialogMethod::Bye).send().await;
    h.advance(Duration::from_millis(75)).await;
    assert_eq!(b2bua.active_calls(), 0, "the call is released before the BYE arrives");
    bob.receive("ACK").await;
    call.expect(486).await;
    bye.expect(481).await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let report = h.finish().await;
    assert_eq!(invite_final_statuses(&report, alice_addr), [486], "one INVITE final");
}

/// The guard: on a confirmed call the callee hangs up, this stack BYEs the
/// caller, and the caller's own BYE crosses it. Her BYE left while the dialog
/// existed and arrives on a leg this stack is ending: 200 (§15.1.2), as is
/// the 200 she gives this stack's BYE.
///
/// Ordering this test depends on (100 ms transit per hop): bob's BYE leaves at
/// t; the stack's BYE toward alice leaves at t+100 and reaches her at t+200;
/// her BYE leaves at t+150 and reaches the stack at t+250.
#[tokio::test(start_paused = true)]
async fn caller_bye_crossing_ours_on_a_confirmed_dialog_keeps_200() {
    let h = Harness::with_transit_delay("b2bua-confirmed-crossing-bye", 100);
    let alice = h.agent("alice", "127.0.0.1:5164").await;
    let bob = h.agent("bob", "127.0.0.1:5174").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5174).start(&h, "b2bua", "127.0.0.1:5184").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = b_inv.dialog();

    let mut b_bye = bob_dialog.bye().await;
    h.advance(Duration::from_millis(150)).await;
    let mut a_bye = alice_dialog.bye().await;
    b_bye.expect(200).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    a_bye.expect(200).await;

    h.advance(Duration::from_secs(1)).await;
    reaped(&b2bua).await;
    let seen = start_lines(&alice);
    assert_eq!(
        seen.iter().filter(|l| l.starts_with("SIP/2.0 481")).count(),
        0,
        "no 481 on a dialog that existed when the BYE left: {seen:?}"
    );
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one record");
    let t = cdrs[0].termination.as_ref().expect("terminated");
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_ne!(t.by_leg.as_deref(), Some("a"), "the callee released first");

    let _report = h.finish().await;
}
