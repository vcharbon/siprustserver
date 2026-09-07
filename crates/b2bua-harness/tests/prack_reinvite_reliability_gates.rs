//! Where RFC 3262 does NOT let a relayed in-dialog request's provisional
//! leave reliably, it leaves as an ordinary provisional (issue 109, gates 2
//! and 3) — the companion of `prack_reinvite_rseq_ownership.rs`, where it
//! does and leaves under this stack's own number.
//!
//! §3: "If the request did not include either a Supported or Require header
//! field indicating this feature, the UAS MUST NOT send the provisional
//! response reliably." The request this stack builds toward the callee may
//! offer `100rel` where the caller's own did not (a declared advertisement),
//! so the callee answers reliably in good faith; the caller never offered it,
//! so toward her the reliability is withdrawn — and the callee's provisional
//! is this stack's to acknowledge, since it is the one that offered.
//!
//! §3 again: the mechanism serves INVITE alone. A reliable provisional to an
//! UPDATE is the responder's own breach (§4 bars `Require: 100rel` from every
//! other request, §7 Table 3 permits `RSeq` only in INVITE responses), and
//! RFC 4320 §4.1 bars this stack from sending ANY non-100 provisional to a
//! non-INVITE: the caller is shown nothing of it, and the UPDATE completes.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::header::{RAck, RSeq, Require, Supported};
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

/// Bob's own sequence — far from anything this stack mints first.
const BOB_RSEQ: u32 = 4711;

fn requires_100rel(resp: &SipResponse) -> bool {
    resp.header::<Require>().and_then(Result::ok).is_some_and(|r| r.contains("100rel"))
}

fn has_rseq(resp: &SipResponse) -> bool {
    resp.header::<RSeq>().is_some()
}

/// A decision that DECLARES `Supported: 100rel` toward the originated face:
/// every request this stack mints or relays toward bob offers the extension,
/// whatever alice's own request offered.
fn decision_offering_100rel_toward_bob(port: u16) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", port);
                r.features.advertise_capabilities = Some(call::features::AdvertiseCapabilitiesFeature {
                    toward_originator: None,
                    toward_originated: Some(call::features::AdvertisedCapabilities {
                        allow: None,
                        supported: Some(vec!["100rel".to_string()]),
                    }),
                });
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Gate 3. Alice re-INVITEs offering nothing; bob is offered `100rel` by this
/// stack's declaration and answers reliably. Alice is shown an ordinary 183 —
/// no `Require`, no `RSeq` — and this stack PRACKs bob itself, naming the
/// number bob stated under bob's re-INVITE CSeq.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   re-INVITE(no 100rel) → [b2bua: Supported:100rel] → 183(100rel,RSeq)
///          ← 183(plain) ; b2bua PRACK → 200(PRACK)
///          → 200(INVITE) → ACK → BYE → 200(BYE)
/// ```
#[tokio::test]
async fn a_provisional_to_a_reinvite_whose_originator_offered_no_100rel_leaves_unreliably() {
    let h = Harness::with_transit_delay("b2bua-prack-reinvite-no-optin", 0);
    let alice = h.agent("alice", "127.0.0.1:5101").await;
    let bob = h.agent("bob", "127.0.0.1:5102").await;
    let b2bua = B2buaSut::builder(decision_offering_100rel_toward_bob(5102))
        .start(&h, "b2bua", "127.0.0.1:5103")
        .await;

    // ── an ordinary call, established without reliability in play ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs, offering NO reliable provisionals ──
    let mut reinv = alice_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE, OPTIONS, UPDATE, INFO, PRACK")
        .send()
        .await;

    // The declaration offers them to bob on this stack's behalf.
    let mut re_uas = bob.receive("INVITE").await;
    assert!(
        re_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "the declared Supported: 100rel reaches the callee on the relayed re-INVITE",
    );
    let bob_reinvite_cseq = re_uas.request().cseq().seq();

    // Bob answers reliably, in good faith.
    re_uas
        .respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .with_sdp(REANSWER)
        .await;

    // Alice is shown an ordinary provisional: she never offered to PRACK one.
    let p183 = reinv.expect(183).await;
    assert!(
        !requires_100rel(&p183),
        "no Require: 100rel toward an originator who offered none (RFC 3262 §3; issue 109)",
    );
    assert!(!has_rseq(&p183), "no RSeq toward an originator who offered none");

    // This stack offered the extension to bob, so it acknowledges him itself,
    // naming HIS number under HIS re-INVITE CSeq (RFC 3262 §4, §7.2).
    let mut bob_prack = bob.receive("PRACK").await;
    let rack = bob_prack.request().header::<RAck>().expect("an RAck").expect("readable RAck");
    assert_eq!(rack.rseq(), BOB_RSEQ, "the PRACK names the number bob stated");
    assert_eq!(rack.seq(), bob_reinvite_cseq, "the PRACK names bob's re-INVITE");
    bob_prack.respond(200, "OK").await;

    // ── bob answers the re-INVITE; alice ACKs the 2xx ──
    let reinvite_cseq = p183.cseq().seq();
    re_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    alice_dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}

/// Gate 2. A reliable provisional to a relayed UPDATE is the responder's own
/// breach; this stack numbers nothing and, being barred from sending a non-100
/// provisional to a non-INVITE at all (RFC 4320 §4.1), shows the originator
/// nothing of it. The UPDATE completes as usual.
///
/// ```text
///   INVITE → 180 → 200 → ACK
///   UPDATE(Supported:100rel) → 183(100rel,RSeq)   [bob's breach, absorbed]
///          → 200(UPDATE) → BYE → 200(BYE)
/// ```
#[tokio::test]
async fn a_reliable_provisional_to_a_relayed_update_reaches_the_originator_as_nothing() {
    let h = Harness::with_transit_delay("b2bua-prack-update-reliable-1xx", 0);
    // Bob's reliable 183 to an UPDATE is the misbehaviour this test exists to
    // absorb; it is waived on bob only, so every B2BUA bind stays gated.
    h.waive(
        WaiverScope::rule(
            "no-reliable-1xx-on-in-dialog",
            "bob deliberately answers an UPDATE with a reliable provisional (RFC 3262 §3/§4) — \
             the peer breach this test exists to absorb",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", "127.0.0.1:5104").await;
    let bob = h.agent("bob", "127.0.0.1:5105").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5105).start(&h, "b2bua", "127.0.0.1:5106").await;

    // ── an ordinary call ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice UPDATEs the session (RFC 3311), advertising 100rel as she may ──
    let mut update = alice_dialog
        .send_request(InDialogMethod::Update)
        .with_sdp(REOFFER)
        .with_header("Supported", "100rel")
        .send()
        .await;
    let mut update_at_bob = bob.receive("UPDATE").await;

    // Bob's breach: a reliable provisional to a non-INVITE.
    update_at_bob
        .respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", &BOB_RSEQ.to_string())
        .await;

    // The UPDATE completes; the NEXT datagram alice sees is its 2xx — the
    // provisional reached her neither reliably nor as a plain 183 (an
    // `expect` fails on any other status first).
    update_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    let final_200 = update.expect(200).await;
    assert!(!requires_100rel(&final_200) && !has_rseq(&final_200), "nothing reliable reaches the originator");

    // ── teardown ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
    b2bua.assert_fully_reaped();
}
