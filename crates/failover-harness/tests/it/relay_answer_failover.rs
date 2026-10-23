//! **A non-INVITE relay answered after the node serving the call died.** The
//! caller's request is relayed to the callee and replicated; the serving node
//! dies; the callee answers once the proxy reads the node dead. The answer
//! follows the one routing rule (ADR-0055 §8): its holder is dead, so it goes
//! to the call's other owner, the survivor, as an INVITE response would. The
//! survivor takes the call over on it and relays it to the caller from the
//! relay the record holds, so the caller has its final without a
//! retransmission and the callee sees the request once.
//!
//! ```text
//!   alice ─REQ─▶ primary ─REQ─▶ bob          (relayed, replicated, held OPEN)
//!               ✗ primary killed, read dead ✗
//!   bob ─200─▶ proxy ─▶ SURVIVOR ─200─▶ alice
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::FailoverHarness;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::inflight_resend::{
    assert_taken_over_once, bring_up, establish, kill_serving_node, reboot_and_reclaim,
    settle_to_one_cdr, split_mut, survivor_of, synchronized_call_ref, REANSWER, REOFFER, REPLICATE,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// The `method` transactions put on the wire toward `to`, told apart by their
/// top-Via branch: a retransmission repeats its transaction (RFC 3261
/// §17.1.2.2) and is not a second request.
fn requests_toward(fh: &FailoverHarness, to: SocketAddr, method: &Method) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Request(r)) if r.method() == method => {
                r.top_via().branch().map(str::to_string)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// The responses to a `method` request put on the wire toward `to`.
fn responses_toward(fh: &FailoverHarness, to: SocketAddr, method: &Method) -> Vec<u16> {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) if r.cseq().method() == method => Some(r.status()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_update_answered_after_the_kill_completes_on_the_survivor() {
    let mut fh = FailoverHarness::new("relay-answer-failover-update", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── Relayed and replicated; the callee holds it open ────────────────────
    let mut update = dialog.send_request(InDialogMethod::Update).with_sdp(REOFFER).send().await;
    let mut at_bob = bob.receive("UPDATE").await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    // ── The callee answers; the survivor takes it and answers the caller ────
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    update.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    assert_eq!(requests_toward(&fh, bob.addr(), &Method::Update), 1, "the callee sees it once");

    // ── The caller's next UPDATE is relayed and answered ────────────────────
    let mut next = dialog.send_request(InDialogMethod::Update).with_sdp(REOFFER).send().await;
    bob.receive("UPDATE").await.respond(200, "OK").with_sdp(REANSWER).await;
    next.expect(200).await;
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Update),
        [200, 200],
        "one final per UPDATE the caller sent",
    );

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn an_info_answered_after_the_kill_completes_on_the_survivor() {
    let mut fh = FailoverHarness::new("relay-answer-failover-info", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    let mut at_bob = bob.receive("INFO").await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    at_bob.respond(200, "OK").await;
    info.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    assert_eq!(requests_toward(&fh, bob.addr(), &Method::Info), 1, "the callee sees it once");

    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(responses_toward(&fh, alice.addr(), &Method::Info), [200], "one final");

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
