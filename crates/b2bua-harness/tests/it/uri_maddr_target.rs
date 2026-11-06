//! A remote target's `maddr` is where its dialog's in-dialog requests go (RFC
//! 3261 §19.1.1, RFC 3263 §4): at the URI's own port, the name in its host never
//! resolved, the URI whole as their Request-URI (§12.2.1.1).

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use std::net::SocketAddr;

/// `addr`'s socket as a target whose host is a name nothing resolves.
fn target_by_maddr(user: &str, addr: SocketAddr) -> String {
    format!("sip:{user}@name.invalid:{};maddr={}", addr.port(), addr.ip())
}

#[tokio::test(start_paused = true)]
async fn a_callee_bye_reaches_a_caller_whose_contact_names_a_maddr() {
    let s = B2buaScene::new("uri-maddr-caller").await;
    let target = target_by_maddr("alice", s.alice.addr());
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_contact(&target)
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
    assert_eq!(
        alice_bye.request().request_uri().to_string(),
        target,
        "the BYE names the caller's remote target, `maddr` included"
    );
    alice_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    assert_eq!(s.b2bua.metrics().outbound_unaddressable_of("request"), 0, "the name is not read");
    s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_caller_bye_and_the_ack_reach_a_callee_whose_contact_names_a_maddr() {
    let s = B2buaScene::new("uri-maddr-callee").await;
    let target = target_by_maddr("bob", s.bob.addr());
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).with_contact(&target).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    let mut bob_bye = s.bob.receive("BYE").await;
    assert_eq!(
        bob_bye.request().request_uri().to_string(),
        target,
        "the BYE names the callee's remote target, `maddr` included"
    );
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    s.finish().await;
}
