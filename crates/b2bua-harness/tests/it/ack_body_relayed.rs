//! RFC 3261 §13.2.2.4 — the ACK a relayed INVITE's 2xx owes is the
//! acknowledging party's own ACK, relayed, so whatever body that party put on it
//! reaches the far party verbatim: same bytes, same `Content-Type`, a
//! `Content-Length` that frames them.
//!
//! §13.2.1 closes the offer/answer round at the 2xx when the INVITE carried the
//! offer, so a body on that round's ACK is neither an offer nor an answer. This
//! stack does not interpret it — and a back-to-back UA that does not interpret a
//! body owes it onward rather than dropping it. That is also the SIP-I/SIP-T
//! reality: an ACK may carry an ISUP payload (RFC 3372/3204) the callee reads.
//!
//! The stray body itself is charged to its author by
//! `ack-body-after-complete-offer-answer`; each lane that carried it is waived,
//! never the audit.
//!
//! The bare-ACK arm is the floor: relaying what the party sent means relaying
//! nothing when it sent nothing.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, WaiverScope};
use sip_message::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// A body no SIP stack interprets — the point is that it still arrives whole.
const CUSTOM: &[u8] = b"\x01\x02opaque payload\xff\x00trailing";
const CUSTOM_TYPE: &str = "application/x-custom";

/// A media type whose PARAMETER is load-bearing: drop the boundary and the
/// receiver cannot split the parts (RFC 3261 §7.4, RFC 5621).
const MULTIPART_TYPE: &str = "multipart/mixed; boundary=unique-boundary-1";
const MULTIPART: &str = "--unique-boundary-1\r\nContent-Type: application/sdp\r\n\r\nv=0\r\no=alice 3 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n--unique-boundary-1--\r\n";

/// Waive the stray-body finding on both lanes that carried it: the party that
/// authored the body, and the relay that owes it onward.
fn waive_stray_ack_body(h: &Harness, author: &str) {
    h.waive(
        WaiverScope::rule(
            "ack-body-after-complete-offer-answer",
            format!(
                "{author} deliberately puts a body on an ACK whose offer/answer round is \
                 closed — the body the relay must carry is what this test asserts"
            ),
        )
        .on_party(author),
    );
    h.waive(
        WaiverScope::rule(
            "ack-body-after-complete-offer-answer",
            "the SUT carries that body verbatim toward the far party: a back-to-back UA owes a \
             body it does not interpret, and authors none of its own",
        )
        .on_party("b2bua"),
    );
}

/// The bytes, media type and framing of the ACK the callee received.
fn body_of(txn: &scenario_harness::ServerTxn) -> (Vec<u8>, Option<String>, Option<String>) {
    let req = txn.request();
    (
        req.body().to_vec(),
        req.raw(HeaderName::ContentType).next().map(str::to_string),
        req.raw(HeaderName::ContentLength).next().map(str::to_string),
    )
}

/// (a) The INITIAL round: the offer travelled in the INVITE and the answer in
/// the 200, so the round is closed — and the caller's ACK repeats the SDP
/// anyway. The callee's ACK carries those exact bytes.
#[tokio::test(start_paused = true)]
async fn an_initial_ack_body_reaches_the_callee_verbatim() {
    let h = Harness::new("b2bua-ack-body-initial");
    waive_stray_ack_body(&h, "alice");
    let alice = h.agent("alice", "127.0.0.1:5341").await;
    let bob = h.agent("bob", "127.0.0.1:5351").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5351).start(&h, "b2bua", "127.0.0.1:5361").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    // Alice's ACK repeats her offer; the callee's ACK is that ACK.
    let mut dialog = call.ack_with(Some(REOFFER)).await;
    let ack = bob.receive("ACK").await;
    let (body, ct, cl) = body_of(&ack);
    assert_eq!(body, REOFFER.as_bytes(), "the callee's ACK carries the caller's bytes");
    assert_eq!(ct.as_deref(), Some("application/sdp"), "and the media type she stated");
    assert_eq!(
        cl.as_deref(),
        Some(REOFFER.len().to_string().as_str()),
        "framed by a Content-Length that matches",
    );
    assert_eq!(
        ack.request().cseq().seq(),
        uas.request().cseq().seq(),
        "§13.2.2.4: on the INVITE's CSeq",
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// (b) The same on a RE-INVITE round, whose 2xx is taken on the answering leg
/// rather than at dialog confirmation.
#[tokio::test(start_paused = true)]
async fn a_reinvite_ack_body_reaches_the_callee_verbatim() {
    let h = Harness::new("b2bua-ack-body-reinvite");
    waive_stray_ack_body(&h, "alice");
    let alice = h.agent("alice", "127.0.0.1:5342").await;
    let bob = h.agent("bob", "127.0.0.1:5352").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5352).start(&h, "b2bua", "127.0.0.1:5362").await;

    let mut dialog = establish(&alice, &bob, &b2bua).await;

    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;

    dialog.ack_for(reinvite_cseq, Some(REANSWER)).await;
    let ack = bob.receive("ACK").await;
    let (body, ct, cl) = body_of(&ack);
    assert_eq!(body, REANSWER.as_bytes(), "the callee's ACK carries the caller's bytes");
    assert_eq!(ct.as_deref(), Some("application/sdp"));
    assert_eq!(cl.as_deref(), Some(REANSWER.len().to_string().as_str()));
    assert_eq!(ack.request().cseq().seq(), reinvite_cseq, "on the re-INVITE's CSeq");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// (c) A body this stack does not interpret at all — not SDP, not text, with
/// bytes outside ASCII. It is relayed whole, under the type its sender stated.
#[tokio::test(start_paused = true)]
async fn a_non_sdp_ack_body_is_relayed_under_its_own_media_type() {
    let h = Harness::new("b2bua-ack-body-custom");
    let alice = h.agent("alice", "127.0.0.1:5343").await;
    let bob = h.agent("bob", "127.0.0.1:5353").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5353).start(&h, "b2bua", "127.0.0.1:5363").await;

    let mut dialog = establish(&alice, &bob, &b2bua).await;

    let mut reinv = dialog.reinvite(Some(REOFFER)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;

    dialog.ack_for_with_body(reinvite_cseq, Some((CUSTOM_TYPE, CUSTOM.to_vec()))).await;
    let ack = bob.receive("ACK").await;
    let (body, ct, cl) = body_of(&ack);
    assert_eq!(body, CUSTOM, "an uninterpreted body crosses byte for byte");
    assert_eq!(ct.as_deref(), Some(CUSTOM_TYPE), "under the type its sender stated");
    assert_eq!(
        cl.as_deref(),
        Some(CUSTOM.len().to_string().as_str()),
        "framed by its own length, not the SDP default",
    );

    // A second round, this time a type whose parameter frames the body itself:
    // the boundary must survive the relay or the parts cannot be split.
    let mut reinv2 = dialog.reinvite(Some(REOFFER)).await;
    let second_cseq = dialog.local_cseq();
    let mut bob_reinv2 = bob.receive("INVITE").await;
    bob_reinv2.respond(200, "OK").with_sdp(REANSWER).await;
    reinv2.expect(200).await;
    dialog
        .ack_for_with_body(second_cseq, Some((MULTIPART_TYPE, MULTIPART.as_bytes().to_vec())))
        .await;
    let ack2 = bob.receive("ACK").await;
    let (body2, ct2, cl2) = body_of(&ack2);
    assert_eq!(body2, MULTIPART.as_bytes(), "every part crosses whole");
    // The parameter's own spelling is what matters, not the optional LWS after
    // the `;` (RFC 3261 §7.3.1): a lost boundary makes the body unsplittable.
    let ct2 = ct2.expect("the relayed ACK states a media type");
    assert!(ct2.starts_with("multipart/mixed"), "the type crosses: {ct2}");
    assert!(ct2.contains("boundary=unique-boundary-1"), "the boundary crosses: {ct2}");
    assert_eq!(cl2.as_deref(), Some(MULTIPART.len().to_string().as_str()));

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// (d) The floor: a bare ACK yields a bare ACK — no body, and no `Content-Type`
/// invented for one.
#[tokio::test(start_paused = true)]
async fn a_bare_caller_ack_yields_a_bare_callee_ack() {
    let h = Harness::new("b2bua-ack-body-bare");
    let alice = h.agent("alice", "127.0.0.1:5344").await;
    let bob = h.agent("bob", "127.0.0.1:5354").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5354).start(&h, "b2bua", "127.0.0.1:5364").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;

    let mut dialog = call.ack().await;
    let ack = bob.receive("ACK").await;
    let (body, ct, cl) = body_of(&ack);
    assert!(body.is_empty(), "nothing sent, nothing relayed");
    assert_eq!(ct, None, "no Content-Type is invented for an absent body");
    assert_eq!(cl.as_deref(), Some("0"), "and the framing says zero");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// INVITE → 180 → 200 → the caller's ACK, relayed. Returns the caller's dialog.
async fn establish(
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    b2bua: &B2buaSut,
) -> scenario_harness::Dialog {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "call established");
    dialog
}
