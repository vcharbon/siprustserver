//! A remote target whose user part holds `;name=value` (RFC 3261 §25.1
//! `user-unreserved`) is a URI like any other: the in-dialog requests of its
//! dialog reach the host and port after the `@`, with that URI, user part
//! whole, as their Request-URI (§12.2.1.1).

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};

const USER: &str = "+33600000000;foo=bar";

/// The caller's Contact names `USER`; the callee hangs up. The caller
/// receives the BYE at its socket and answers it, both dialogs end, and the
/// call writes its CDR and is reaped.
#[tokio::test(start_paused = true)]
async fn a_callee_bye_reaches_a_caller_whose_contact_user_holds_a_parameter() {
    let s = B2buaScene::new("remote-target-user-param-caller").await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_contact(format!("sip:{USER}@{}", s.alice.addr()))
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut bye = bob_dialog.bye().await;
    let mut alice_bye = s.alice.receive("BYE").await;
    let alice = s.alice.addr();
    assert_eq!(
        alice_bye.request().request_uri().to_string(),
        format!("sip:{USER}@{}:{}", alice.ip(), alice.port()),
        "the BYE names the caller's remote target, user part whole"
    );
    alice_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    s.finish().await;
}

/// The callee's 2xx Contact names `USER`; the caller hangs up. The callee
/// receives the BYE at its socket and answers it.
#[tokio::test(start_paused = true)]
async fn a_caller_bye_reaches_a_callee_whose_contact_user_holds_a_parameter() {
    let s = B2buaScene::new("remote-target-user-param-callee").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK")
        .with_sdp(ANSWER_SDP)
        .with_contact(&format!("sip:{USER}@{}", s.bob.addr()))
        .await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    let mut bob_bye = s.bob.receive("BYE").await;
    let bob = s.bob.addr();
    assert_eq!(
        bob_bye.request().request_uri().to_string(),
        format!("sip:{USER}@{}:{}", bob.ip(), bob.port()),
        "the BYE names the callee's remote target, user part whole"
    );
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    s.finish().await;
}
