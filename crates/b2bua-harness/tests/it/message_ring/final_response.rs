//! The final-response header list (`CdrConfig::final_response_headers`): an
//! INVITE final's ring entry, answer, rejection or the 487 a CANCEL draws,
//! captures exactly the names it lists; every other entry, a provisional and
//! the 200 to a CANCEL among them, the common `captured_headers`.

use super::*;

const AGENT: &str = "Callee-UA/1.0 (ring test)";

/// The common list `Allow`, the final one `User-Agent` alone.
fn split_lists() -> CdrConfig {
    CdrConfig {
        message_ring: 32,
        captured_headers: vec!["Allow".into()],
        final_response_headers: Some(vec!["User-Agent".into()]),
    }
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

/// The b-leg's received INVITE response with `code`.
fn b_response(call: &Call, code: u16) -> &MessageEntry {
    b_leg(call)
        .messages
        .entries
        .iter()
        .find(|e| e.direction == Received && e.method == "INVITE" && e.code == Some(code))
        .unwrap_or_else(|| panic!("the b-leg holds bob's {code}"))
}

#[tokio::test(start_paused = true)]
async fn an_answer_captures_the_final_list_and_its_provisional_the_common_one() {
    let h = Harness::new("message-ring-final-answer");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, split_lists()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing")
        .with_header("Allow", "INVITE, ACK, BYE, CANCEL")
        .with_header("User-Agent", AGENT)
        .await;
    call.expect(180).await;
    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header("Allow", "INVITE, ACK, BYE, CANCEL")
        .with_header("User-Agent", AGENT)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").with_header("User-Agent", AGENT).await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(b_response(&done, 180).headers, vec![pair("Allow", "INVITE, ACK, BYE, CANCEL")]);
    assert_eq!(b_response(&done, 200).headers, vec![pair("User-Agent", AGENT)]);
    let bye_answer = b_leg(&done)
        .messages
        .entries
        .iter()
        .find(|e| e.method == "BYE" && e.code == Some(200))
        .expect("bob's 200 to the BYE");
    assert!(bye_answer.headers.is_empty(), "a BYE's answer is no INVITE final");

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_rejection_captures_the_final_list() {
    let h = Harness::new("message-ring-final-reject");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, split_lists()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here")
        .with_header("Allow", "INVITE, ACK, BYE, CANCEL")
        .with_header("User-Agent", AGENT)
        .await;
    bob.receive("ACK").await;
    call.expect(486).await;

    let done = sut.assert_reaped().await;
    assert_eq!(b_response(&done, 486).headers, vec![pair("User-Agent", AGENT)]);

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_487_a_cancel_draws_captures_the_final_list_and_the_cancels_200_the_common_one() {
    let h = Harness::new("message-ring-final-cancel");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, split_lists()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    bob_cxl
        .respond(200, "OK")
        .with_header("Allow", "INVITE, ACK, BYE, CANCEL")
        .with_header("User-Agent", AGENT)
        .await;
    uas.respond(487, "Request Terminated")
        .with_header("Allow", "INVITE, ACK, BYE, CANCEL")
        .with_header("User-Agent", AGENT)
        .await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;

    let done = sut.assert_reaped().await;
    assert_eq!(b_response(&done, 487).headers, vec![pair("User-Agent", AGENT)]);
    let cancel_ok = b_leg(&done)
        .messages
        .entries
        .iter()
        .find(|e| e.method == "CANCEL" && e.code == Some(200))
        .expect("bob's 200 to the CANCEL");
    assert_eq!(cancel_ok.headers, vec![pair("Allow", "INVITE, ACK, BYE, CANCEL")]);

    let _report = h.finish().await;
}
