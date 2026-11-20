//! A relayed PRACK the far leg answers with a failure final.
//!
//! The caller's PRACK names the reliable provisional this stack showed her, so
//! this stack, the UAS of her leg, owes it a 2xx (RFC 3262 §3) whatever the far
//! leg makes of the relayed copy: a failure final there draws the caller's
//! 200, and only one. A PRACK carrying an offer of the caller's (RFC 3262 §5)
//! has its failure relayed instead: the offer is refused end to end.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::dialogs::{DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP};
use scenario_harness::{Agent, Harness, RunReport, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_net::{Arrival, PreIngressAction};

use crate::common::release_callee as release;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// Alice's second offer, in her PRACK: the next version of her session.
const PRACK_OFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
/// Alice's offer after the PRACK's was refused, in an early UPDATE.
const UPDATE_OFFER: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
/// Bob's answer to the UPDATE's offer.
const UPDATE_ANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// Bob's offer, in his reliable 183 to an INVITE that carried none.
const EARLY_OFFER: &str = "v=0\r\no=bob 2 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";
/// Alice's answer to [`EARLY_OFFER`], in her PRACK.
const EARLY_ANSWER: &str = "v=0\r\no=alice 2 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10006 RTP/AVP 0\r\n";

/// Bob's own `RSeq`.
const BOB_RSEQ: u32 = 4711;

/// A callee every PRACK reaching which is lost while `lost` holds.
async fn callee_losing_pracks(h: &Harness, lost: Arc<AtomicBool>) -> Agent {
    h.agent_with_pre_ingress(
        "bob",
        &h.next_addr().to_string(),
        Arc::new(move |arrival: &mut Arrival, _src, _load| {
            if arrival.raw().starts_with(b"PRACK ") && lost.load(Ordering::SeqCst) {
                return PreIngressAction::Drop;
            }
            PreIngressAction::Accept
        }),
    )
    .await
}

/// Bob's waivers for a PRACK lost toward him and overtaken there by an UPDATE
/// whose CSeq is above it: the gap, then the retransmission's CSeq gone back.
fn waive_overtaken_prack(h: &Harness) {
    h.waive(
        WaiverScope::rule(DIALOG_CSEQ_SKIP, "the PRACK lost toward bob is the stimulus")
            .on_party("bob"),
    );
    h.waive(
        WaiverScope::rule(
            DIALOG_CSEQ_BACKWARDS,
            "the PRACK's retransmission reaches bob after the UPDATE: its 500 is the stimulus",
        )
        .on_party("bob"),
    );
}

/// Bob's waiver for denying 481 a PRACK whose reliable 183 he still holds
/// unacknowledged: RFC 3262 §3 owes that PRACK a 2xx.
fn waive_prack_denied_481(h: &Harness) {
    h.waive(
        WaiverScope::rule(
            "prack-2xx-or-481",
            "bob denies 481 a PRACK for the reliable 183 he still holds unacknowledged: \
             the 481 is the stimulus",
        )
        .on_party("bob"),
    );
}

/// The finals sent to `to` for PRACK transactions, each copy counted.
fn prack_finals(report: &RunReport, to: SocketAddr) -> Vec<u16> {
    report
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match CustomParser::built().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() >= 200 && *r.cseq().method() == Method::Prack =>
            {
                Some(r.status())
            }
            _ => None,
        })
        .collect()
}

/// The relayed PRACK is lost toward bob, the caller's early UPDATE overtakes
/// it there, and its retransmission arrives with a CSeq gone backwards: bob
/// answers it 500 (RFC 3261 §12.2.2). The caller's PRACK draws 200, the call
/// is answered, and a service then releases bob's leg: the release answers
/// nothing more on the caller's PRACK, already answered. Her BYE ends the call.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_refused_by_the_callee_draws_the_caller_s_200() {
    let h = Harness::with_transit_delay("relayed-prack-refused", 1);
    waive_overtaken_prack(&h);
    let lost = Arc::new(AtomicBool::new(true));
    let alice = h.agent_auto("alice").await;
    let alice_addr = alice.addr();
    let bob = callee_losing_pracks(&h, lost.clone()).await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .services(vec![release::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");

    let mut update = call.send_request(InDialogMethod::Update).send().await;
    bob.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;

    lost.store(false, Ordering::SeqCst);
    bob.receive("PRACK").await.respond(500, "Server Internal Error").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(release::AT_SEC as u64)).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    h.advance(Duration::from_secs(1)).await;

    let mut bye = dialog.bye().await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        prack_finals(&report, alice_addr),
        [200],
        "the caller's PRACK draws one final (RFC 3261 §17.2.1): the release of the leg its \
         relay went to answers it no second time",
    );
}

/// Bob answers the relayed PRACK 481 at once. The caller's PRACK draws 200,
/// and the call is answered and ends normally.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_denied_481_by_the_callee_draws_the_caller_s_200() {
    let h = Harness::with_transit_delay("relayed-prack-481", 1);
    waive_prack_denied_481(&h);
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    bob.receive("PRACK").await.respond(481, "Call/Transaction Does Not Exist").await;
    prack.expect(200).await;

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "a PRACK's 481 denies no dialog (RFC 3262 §3)");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// Alice's INVITE carries no offer, bob's reliable 183 carries his, and her
/// PRACK carries her answer (RFC 3262 §5). Bob denies the relayed PRACK 481:
/// alice's PRACK draws one final, a 200, since it offered nothing to refuse.
/// Bob may not answer the INVITE 2xx with his offer's 183 unacknowledged
/// (RFC 3262 §3): alice CANCELs.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_answering_the_callee_s_offer_denied_481_draws_the_caller_s_200() {
    let h = Harness::with_transit_delay("relayed-prack-answer-481", 1);
    waive_prack_denied_481(&h);
    let alice = h.agent_auto("alice").await;
    let alice_addr = alice.addr();
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert!(uas.request().sdp().is_none(), "alice's INVITE reaches bob without an offer");
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(EARLY_OFFER).await;
    let p183 = call.expect(183).await;
    assert!(p183.sdp().is_some(), "bob's offer reaches alice in the reliable 183");
    let rack = format!(
        "{} {} INVITE",
        p183.header::<sip_message::header::RSeq>().expect("RSeq").expect("RSeq").value(),
        p183.cseq().seq()
    );
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&rack)
        .with_sdp(EARLY_ANSWER)
        .send()
        .await;
    bob.receive("PRACK").await.respond(481, "Call/Transaction Does Not Exist").await;
    prack.expect(200).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    call.expect(487).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let report = h.finish().await;
    assert_eq!(
        prack_finals(&report, alice_addr),
        [200],
        "a PRACK carrying the answer to the callee's offer draws the caller's 200, \
         never the callee's refusal (RFC 3262 §3)",
    );
}

/// Bob's reliable 183 answers alice's offer, and her PRACK carries a new one
/// (RFC 3262 §5). The relayed PRACK is overtaken toward bob as above and bob
/// answers it 500: alice gets that 500, her offer refused end to end. Her
/// next offer, in an early UPDATE, is relayed and answered. She then CANCELs.
#[tokio::test(start_paused = true)]
async fn a_relayed_prack_offer_refused_by_the_callee_is_refused_to_the_caller() {
    let h = Harness::with_transit_delay("relayed-prack-offer-refused", 1);
    waive_overtaken_prack(&h);
    h.waive(
        WaiverScope::rule(
            "prack-2xx-or-481",
            "the stack relays bob's failure to alice's offer-carrying PRACK: the offer is \
             refused end to end",
        )
        .on_party("b2bua"),
    );
    let lost = Arc::new(AtomicBool::new(true));
    let alice = h.agent_auto("alice").await;
    let bob = callee_losing_pracks(&h, lost.clone()).await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(BOB_RSEQ).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let rack = format!(
        "{} {} INVITE",
        p183.header::<sip_message::header::RSeq>().expect("RSeq").expect("RSeq").value(),
        p183.cseq().seq()
    );
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_rack(&rack)
        .with_sdp(PRACK_OFFER)
        .send()
        .await;

    let mut update = call.send_request(InDialogMethod::Update).send().await;
    bob.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;

    lost.store(false, Ordering::SeqCst);
    bob.receive("PRACK").await.respond(500, "Server Internal Error").await;
    prack.expect(500).await;

    let mut reoffer = call.send_request(InDialogMethod::Update).with_sdp(UPDATE_OFFER).send().await;
    let mut reoffer_at_bob = bob.receive("UPDATE").await;
    assert!(reoffer_at_bob.request().sdp().is_some(), "alice's next offer reaches bob");
    reoffer_at_bob.respond(200, "OK").with_sdp(UPDATE_ANSWER).await;
    let answered = reoffer.expect(200).await;
    assert!(answered.sdp().is_some(), "bob's answer reaches alice");

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    call.expect(487).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
