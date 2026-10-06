//! An offer-bearing UPDATE that arrives while its sender's previous offer is
//! still unanswered here is refused 500 with a `Retry-After` of 0 to 10 s
//! (RFC 3311 §5.2): the stack has received an offer it has not answered. The
//! previous offer may ride an UPDATE or an INVITE. It is never relayed, so the
//! peer never holds two offers at once (RFC 3264 §4).

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, stated_by_response, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";
const REOFFER2: &str = "v=0\r\no=bob 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";

/// The `Retry-After` seconds `refused` carries, which RFC 3311 §5.2 bounds.
fn retry_after_sec(refused: &SipResponse) -> u32 {
    let secs: u32 = stated_by_response(refused, "Retry-After")
        .expect("the 500 carries a Retry-After")
        .parse()
        .expect("delta-seconds");
    assert!(secs <= 10, "{secs}");
    secs
}

/// Bob offers in an UPDATE, relayed to alice and unanswered, then offers again
/// in a second UPDATE: 500, and alice never sees it.
#[tokio::test]
async fn a_second_offer_while_the_first_update_is_unanswered_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-offer-pending", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "bob deliberately offers again while his first offer is unanswered — the \
             peer behaviour this test exists to refuse",
        )
        .on_party("bob")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6321").await;
    let bob = h.agent("bob", "127.0.0.1:6322").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6322).start(&h, "b2bua", "127.0.0.1:6323").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut u1 = bob_dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    let mut alice_u1 = alice.receive("UPDATE").await;
    let mut u2 = bob_dialog.request(InDialogMethod::Update, Some(REOFFER2)).await;
    retry_after_sec(&u2.expect(500).await);

    alice_u1.respond(200, "OK").with_sdp(REANSWER).await;
    u1.expect(200).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let updates_to_alice = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.raw.starts_with(b"UPDATE "))
        .count();
    assert_eq!(updates_to_alice, 1, "only the first offer crossed the bridge");
}

/// Bob offers in a re-INVITE, relayed to alice and unanswered, then offers in
/// an UPDATE: 500.
#[tokio::test]
async fn an_update_offer_while_the_sender_reinvite_is_unanswered_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-offer-over-invite", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "bob deliberately offers in an UPDATE while his re-INVITE offer is unanswered",
        )
        .on_party("bob")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6324").await;
    let bob = h.agent("bob", "127.0.0.1:6325").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6325).start(&h, "b2bua", "127.0.0.1:6326").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let cseq = reinv.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER2)).await;
    retry_after_sec(&update.expect(500).await);

    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack_for(cseq, None).await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// Bob offers in an UPDATE and sends a bodyless UPDATE before the first is
/// answered: RFC 3311 §5.2 refuses any UPDATE over a pending one, body or not.
#[tokio::test]
async fn a_bodyless_update_while_the_first_is_unanswered_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-bodyless-over-update", 0);
    let alice = h.agent("alice", "127.0.0.1:6331").await;
    let bob = h.agent("bob", "127.0.0.1:6332").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6332).start(&h, "b2bua", "127.0.0.1:6333").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut u1 = bob_dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    let mut alice_u1 = alice.receive("UPDATE").await;
    let mut u2 = bob_dialog.request(InDialogMethod::Update, None).await;
    retry_after_sec(&u2.expect(500).await);

    alice_u1.respond(200, "OK").with_sdp(REANSWER).await;
    u1.expect(200).await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// A re-INVITE offering `100rel` whose offer no reliable provisional answered
/// (alice sends an unreliable 180): bob's UPDATE offer overlaps it, 500.
#[tokio::test]
async fn an_update_offer_over_a_100rel_reinvite_with_no_reliable_answer_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-offer-over-100rel-invite", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "bob deliberately offers in an UPDATE while his re-INVITE offer is unanswered",
        )
        .on_party("bob")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6334").await;
    let bob = h.agent("bob", "127.0.0.1:6335").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6335).start(&h, "b2bua", "127.0.0.1:6336").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_header("Supported", "100rel")
        .with_sdp(REOFFER)
        .send()
        .await;
    let cseq = reinv.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    alice_uas.respond(180, "Ringing").await;
    reinv.expect(180).await;
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER2)).await;
    retry_after_sec(&update.expect(500).await);

    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack_for(cseq, None).await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The legal nesting of RFC 3311 §5.1: alice answers bob's re-INVITE offer in a
/// reliable 183, bob PRACKs it, and bob's UPDATE offer is relayed.
#[tokio::test]
async fn an_update_offer_after_a_reliable_answer_to_the_reinvite_is_relayed() {
    let h = Harness::with_transit_delay("b2bua-update-offer-after-reliable-answer", 0);
    let alice = h.agent("alice", "127.0.0.1:6337").await;
    let bob = h.agent("bob", "127.0.0.1:6338").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6338).start(&h, "b2bua", "127.0.0.1:6339").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_header("Supported", "100rel")
        .with_sdp(REOFFER)
        .send()
        .await;
    let cseq = reinv.sent_invite().expect("the re-INVITE bob just sent").cseq().seq();
    let mut alice_uas = alice.receive("INVITE").await;
    alice_uas.respond(183, "Session Progress").reliable(4711).with_sdp(REANSWER).await;
    let early = reinv.expect(183).await;
    let rseq = stated_by_response(&early, "RSeq").expect("a reliable 183");
    let mut prack = bob_dialog
        .send_request(InDialogMethod::Prack)
        .with_rack(&format!("{rseq} {cseq} INVITE"))
        .send()
        .await;
    alice.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER2)).await;
    let mut alice_update = alice.receive("UPDATE").await;
    alice_update.respond(200, "OK").with_sdp(REANSWER).await;
    update.expect(200).await;

    alice_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack_for(cseq, None).await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The caller offers in an early-dialog UPDATE while her INVITE's offer is
/// unanswered (bob sent an unreliable 180): 500, and bob never sees it.
#[tokio::test]
async fn an_early_update_offer_over_the_unanswered_initial_invite_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-offer-over-initial-invite", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "alice deliberately offers in an UPDATE while her INVITE offer is unanswered",
        )
        .on_party("alice")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6340").await;
    let bob = h.agent("bob", "127.0.0.1:6341").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6341).start(&h, "b2bua", "127.0.0.1:6342").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let a_tag = ringing.to().tag().expect("an early dialog").to_string();

    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&a_tag)
        .with_sdp(REANSWER)
        .send()
        .await;
    retry_after_sec(&update.expect(500).await);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let updates_to_bob = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua.addr && e.raw.starts_with(b"UPDATE "))
        .count();
    assert_eq!(updates_to_bob, 0, "the refused offer never crossed the bridge");
}

/// Bob CANCELs his re-INVITE (487) and offers in an UPDATE while the relayed
/// copy still awaits alice's final. His own offer is over, so no 500; but the
/// stack's offer toward alice is still open, so relaying would put a second
/// offer on her dialog (RFC 3264 §4): 491, no Retry-After.
#[tokio::test]
async fn an_update_offer_after_its_cancelled_reinvite_is_refused_491() {
    let h = Harness::with_transit_delay("b2bua-update-offer-after-cancel", 0);
    let alice = h.agent("alice", "127.0.0.1:6343").await;
    let bob = h.agent("bob", "127.0.0.1:6344").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6344).start(&h, "b2bua", "127.0.0.1:6345").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinv = bob_dialog.reinvite(Some(REOFFER)).await;
    let mut alice_uas = alice.receive("INVITE").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    reinv.expect(487).await;

    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER2)).await;
    let refused = update.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    alice_uas.respond(100, "Trying").await;
    alice.receive("CANCEL").await.respond(200, "OK").await;
    alice_uas.respond(487, "Request Terminated").await;
    alice.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The count of UPDATEs the stack sent to `to`.
fn updates_to(report: &scenario_harness::RunReport, from: std::net::SocketAddr, to: &str) -> usize {
    let to: std::net::SocketAddr = to.parse().expect("an address");
    report
        .entries()
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"UPDATE "))
        .count()
}

/// alice's INVITE offers `100rel` and an offer; bob's reliable 180 carries no
/// description, so nothing answered her offer: after her PRACK, her UPDATE
/// offer still overlaps it (RFC 3311 §5.2), 500, and bob never sees it.
#[tokio::test]
async fn an_update_offer_after_a_reliable_provisional_without_sdp_is_refused_500() {
    let h = Harness::with_transit_delay("b2bua-update-offer-after-bodyless-reliable", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "alice deliberately offers in an UPDATE while her INVITE offer is unanswered",
        )
        .on_party("alice")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6346").await;
    let bob = h.agent("bob", "127.0.0.1:6347").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6347).start(&h, "b2bua", "127.0.0.1:6348").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").reliable(1).await;
    let ringing = call.expect(180).await;
    let a_tag = ringing.to().tag().expect("an early dialog").to_string();
    let rseq = stated_by_response(&ringing, "RSeq").expect("a reliable 180");
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{rseq} 1 INVITE"))
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&a_tag)
        .with_sdp(REANSWER)
        .send()
        .await;
    retry_after_sec(&update.expect(500).await);

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(updates_to(&report, b2bua.addr, "127.0.0.1:6347"), 0);
}

/// alice's INVITE carries no offer; bob offers in a reliable 183, relayed to
/// her. Before she answers it in her PRACK, she offers in an UPDATE: the
/// stack's offer to her is open (RFC 3311 §5.2), 491, and bob never sees it.
#[tokio::test]
async fn an_update_offer_while_the_offer_in_a_reliable_183_is_unanswered_is_refused_491() {
    let h = Harness::with_transit_delay("b2bua-update-offer-over-183-offer", 0);
    h.waive(
        WaiverScope::rule(
            "no-new-offer-while-offer-pending",
            "alice deliberately offers in an UPDATE before answering the offer she was sent",
        )
        .on_party("alice")
        .conditional(),
    );
    let alice = h.agent("alice", "127.0.0.1:6349").await;
    let bob = h.agent("bob", "127.0.0.1:6350").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 6350).start(&h, "b2bua", "127.0.0.1:6351").await;

    let mut call =
        alice.invite(&bob).with_header("Supported", "100rel").through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let early = call.expect(183).await;
    let a_tag = early.to().tag().expect("an early dialog").to_string();
    let rseq = stated_by_response(&early, "RSeq").expect("a reliable 183");

    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&a_tag)
        .with_sdp(REANSWER)
        .send()
        .await;
    let refused = update.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{rseq} 1 INVITE"))
        .with_sdp(OFFER)
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(updates_to(&report, b2bua.addr, "127.0.0.1:6350"), 0);
}

/// carol's reliable 183 answered alice's offer, then carol fails and the call
/// reroutes to bob, who rings unreliably under a fresh a-face tag. In that
/// early dialog nothing answered alice's offer: 500. In carol's early dialog it
/// was answered, so her UPDATE offer there is legal, but the stack's INVITE
/// offer to bob is unanswered and cannot be relayed (RFC 3264 §4): 491. bob
/// never sees either.
#[tokio::test(start_paused = true)]
async fn an_update_offer_after_a_reroute_whose_new_leg_has_not_answered_is_refused_491() {
    let h = Harness::new("b2bua-update-offer-after-reroute");
    let alice = h.agent("alice", "127.0.0.1:6352").await;
    let carol = h.agent("carol", "127.0.0.1:6353").await;
    let bob = h.agent("bob", "127.0.0.1:6354").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 6353);
                r.callback_context = Some("ctx-update-reroute".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 6354)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:6355").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let early = call.expect(183).await;
    let a_tag = early.to().tag().expect("an early dialog").to_string();
    let rseq = stated_by_response(&early, "RSeq").expect("a reliable 183");
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag)
        .with_rack(&format!("{rseq} 1 INVITE"))
        .send()
        .await;
    carol.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;
    carol_uas.respond(486, "Busy Here").await;
    carol.receive("ACK").await;

    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let tag = ringing.to().tag().expect("an early dialog").to_string();

    assert_ne!(tag, a_tag, "the rerouted leg rings under a fresh a-face tag");

    let mut fresh =
        call.send_request(InDialogMethod::Update).with_to_tag(&tag).with_sdp(REANSWER).send().await;
    retry_after_sec(&fresh.expect(500).await);

    let mut answered = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&a_tag)
        .with_sdp(REANSWER)
        .send()
        .await;
    let refused = answered.expect(491).await;
    assert_eq!(stated_by_response(&refused, "Retry-After"), None);

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(updates_to(&report, b2bua.addr, "127.0.0.1:6354"), 0);
}
