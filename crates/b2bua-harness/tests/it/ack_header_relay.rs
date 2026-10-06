//! The ACK for a 2xx the back-to-back UA relays is the acknowledging party's own
//! ACK (RFC 3261 §13.2.2.4): a request of its own transaction, carrying the
//! end-to-end headers that party put on it, filtered like any relayed request
//! (§16.6) — the deployment's relay policy, the privacy-service role and the
//! call's stated headers all apply. An advertisement header RFC 3261 §20
//! Tables 2 and 3 mark "not applicable" on an ACK (Allow, Supported, the Accept
//! family) is left behind. An ACK the stack composes on its own account relays
//! nothing of a peer's.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{HeaderUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use call::features::{RelayFirst18xStrategy, StatedHeaders};
use sip_message::generators::{InDialogMethod, MessageClass, RelayDirection, RelayPolicy};
use sip_message::header::HeaderName;
use sip_message::method::Method;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

const PAI_LINE: &str = "<sip:+15550001@op.example>";

/// The end-to-end lines an acknowledging party states on its ACK: its product
/// banner, its privacy request beside the asserted identity it covers, user
/// data for the far endpoint, its event packages and a vendor extension.
const ACK_LINES: &[(&str, &str)] = &[
    ("User-Agent", "AliceSoft/1.0"),
    ("Privacy", "id"),
    ("P-Asserted-Identity", PAI_LINE),
    ("User-to-User", "3132333435;encoding=hex"),
    ("Allow-Events", "telephone-event"),
    ("X-Vendor-Note", "acked"),
];

/// The advertisement lines RFC 3261 §20 Tables 2 and 3 mark "not applicable"
/// on an ACK, which a peer may still put on it.
const NOT_APPLICABLE_ON_ACK: &[(&str, &str)] = &[
    ("Allow", "INVITE, ACK, BYE, CANCEL, OPTIONS"),
    ("Supported", "timer, replaces"),
    ("Accept", "application/sdp"),
    ("Accept-Language", "en"),
];

fn lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// Alice calls bob, bob answers, and alice's ACK states `ack_lines`; returns
/// the ACK bob receives and alice's confirmed dialog.
async fn answered_and_acked(
    s: &B2buaScene,
    ack_lines: &[(&str, &str)],
) -> (SipRequest, scenario_harness::Dialog) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack_stating(ack_lines).await;
    let ack = s.bob.receive("ACK").await.request().clone();
    (ack, dialog)
}

/// Every end-to-end line alice put on her ACK reaches bob on the ACK minted
/// for his leg. As the privacy service (the default) the asserted identity
/// her `Privacy: id` covers stays behind; the request itself rides.
#[tokio::test(start_paused = true)]
async fn a_callers_ack_headers_ride_the_ack_minted_for_the_callee() {
    let s = B2buaScene::new("b2bua-ack-header-relay").await;
    let (ack, mut dialog) = answered_and_acked(&s, ACK_LINES).await;

    for (name, value) in ACK_LINES.iter().filter(|(n, _)| *n != "P-Asserted-Identity") {
        assert_eq!(lines(&ack, name), [*value], "{name} rides the callee's ACK");
    }
    assert!(
        lines(&ack, "P-Asserted-Identity").is_empty(),
        "the privacy service leaves the covered identity behind (RFC 3325 §7)"
    );
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// An Allow, Supported or Accept family line the acknowledging party put on
/// its ACK stays behind (RFC 3261 §20: "not applicable" there, a sender MUST
/// NOT place it); the lines beside it still ride.
#[tokio::test(start_paused = true)]
async fn an_advertisement_the_rfc_excludes_from_an_ack_stays_behind() {
    let s = B2buaScene::new("b2bua-ack-header-relay-not-applicable").await;
    let stated: Vec<(&str, &str)> =
        ACK_LINES.iter().chain(NOT_APPLICABLE_ON_ACK).copied().collect();
    let (ack, mut dialog) = answered_and_acked(&s, &stated).await;

    for (name, _) in NOT_APPLICABLE_ON_ACK {
        assert!(lines(&ack, name).is_empty(), "{name} stays behind on the ACK: {ack:?}");
    }
    assert_eq!(lines(&ack, "Allow-Events"), ["telephone-event"]);
    assert_eq!(lines(&ack, "User-Agent"), ["AliceSoft/1.0"]);
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// Inside the trust domain the asserted identity rides the ACK beside the
/// privacy request, as on every other relayed request.
#[tokio::test(start_paused = true)]
async fn inside_the_trust_domain_the_asserted_identity_rides_the_ack() {
    let s = B2buaScene::with_b2bua("b2bua-ack-header-relay-trusted", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.privacy_service = false)
    })
    .await;
    let (ack, mut dialog) = answered_and_acked(&s, ACK_LINES).await;

    assert_eq!(lines(&ack, "P-Asserted-Identity"), [PAI_LINE]);
    assert_eq!(lines(&ack, "Privacy"), ["id"]);

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// The callee's ACK to the 2xx answering its re-INVITE reaches the caller
/// with the callee's end-to-end lines.
#[tokio::test(start_paused = true)]
async fn a_callees_ack_to_its_reinvite_rides_toward_the_caller() {
    let s = B2buaScene::new("b2bua-ack-header-relay-callee").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut alice_reinv = s.alice.receive("INVITE").await;
    alice_reinv.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack_stating(&[("User-Agent", "BobPhone/2.0"), ("X-Vendor-Note", "re-acked")]).await;

    let ack = s.alice.receive("ACK").await;
    assert_eq!(lines(ack.request(), "User-Agent"), ["BobPhone/2.0"]);
    assert_eq!(lines(ack.request(), "X-Vendor-Note"), ["re-acked"]);

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// A relay policy entry naming the ACK leaves its header behind on the ACK,
/// in the direction it names; the other lines still ride.
#[tokio::test(start_paused = true)]
async fn a_relay_policy_entry_for_the_ack_leaves_its_header_behind() {
    let policy = RelayPolicy::transparent().dropping(
        "User-to-User",
        MessageClass::Request(Method::Ack),
        Some(RelayDirection::TowardCallee),
    );
    let s = B2buaScene::with_b2bua("b2bua-ack-header-relay-policy", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(move |c| c.relay_policy = policy)
    })
    .await;
    let (ack, mut dialog) = answered_and_acked(&s, ACK_LINES).await;

    assert!(lines(&ack, "User-to-User").is_empty(), "the entry drops it on the ACK");
    assert_eq!(lines(&ack, "X-Vendor-Note"), ["acked"], "an unnamed line still rides");

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// The call's every-message statement lands on the relayed ACK beside the
/// caller's own lines.
#[tokio::test(start_paused = true)]
async fn an_every_message_statement_lands_beside_the_callers_ack_lines() {
    let s = B2buaScene::with_b2bua("b2bua-ack-header-relay-stated", |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                let mut stated = StatedHeaders::default();
                stated
                    .every_message
                    .insert("X-Stated".into(), HeaderUpdate::Set(vec!["every".into()]));
                route.features.stated_headers = Some(stated);
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await;
    let (ack, mut dialog) = answered_and_acked(&s, ACK_LINES).await;

    assert_eq!(lines(&ack, "X-Stated"), ["every"]);
    assert_eq!(lines(&ack, "X-Vendor-Note"), ["acked"]);

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// Under promote-PEM the caller's ACK closes the promoted 200 and is absorbed;
/// the callee's 2xx is ACKed on the stack's own account, with none of the
/// caller's lines.
#[tokio::test(start_paused = true)]
async fn the_stacks_own_ack_carries_none_of_the_callers_lines() {
    let s = B2buaScene::with_b2bua("b2bua-ack-header-relay-own", |bob_port| {
        B2buaSut::route_all_to_with_18x(
            "127.0.0.1",
            bob_port,
            RelayFirst18xStrategy::PromotePemTo200,
        )
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("P-Early-Media", "sendrecv")
        .with_sdp(ANSWER)
        .await;
    call.expect(200).await;
    let mut dialog = call.ack_stating(ACK_LINES).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;

    let ack = s.bob.receive("ACK").await;
    for (name, _) in ACK_LINES {
        assert!(lines(ack.request(), name).is_empty(), "{name} is the caller's, not ours");
    }

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// RFC 3261 §13.2.2.4: a retransmitted 2xx is re-ACKed with THE ACK, so the
/// repeat carries the caller's lines as the first copy did.
#[tokio::test(start_paused = true)]
async fn the_re_ack_of_a_retransmitted_2xx_carries_the_callers_lines() {
    let s = B2buaScene::new("b2bua-ack-header-relay-re-ack").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack_stating(ACK_LINES).await;
    s.bob.receive("ACK").await;

    // bob never saw it: his 2xx comes again and the SUT re-passes the ACK.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let mut re_acks = 0;
    for _ in 0..10 {
        s.h.advance(Duration::from_millis(100)).await;
        re_acks += s.bob.drain().await;
        if re_acks >= 1 {
            break;
        }
    }
    assert_eq!(re_acks, 1, "the SUT re-ACKs the retransmitted 2xx");

    s.hangup(&mut dialog).await;
    let (b2bua, bob) = (s.b2bua.addr, s.bob.addr());
    let report = s.finish().await;
    let acks: Vec<SipRequest> = report
        .entries()
        .iter()
        .filter(|e| e.from == b2bua && e.to == bob)
        .filter_map(|e| match CustomParser::new().parse(&e.raw).expect("parseable SIP") {
            SipMessage::Request(r) if r.method() == Method::Ack => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(acks.len(), 2, "the ACK and its repeat");
    for ack in &acks {
        assert_eq!(lines(ack, "User-Agent"), ["AliceSoft/1.0"]);
        assert_eq!(lines(ack, "X-Vendor-Note"), ["acked"]);
    }
}

/// A delayed offer: bob offers in his 200 and alice's ACK carries the answer
/// (RFC 3264 §4) beside her own lines; the ACK minted for bob carries both.
#[tokio::test(start_paused = true)]
async fn a_delayed_offer_ack_carries_its_answer_and_the_callers_lines() {
    let s = B2buaScene::new("b2bua-ack-header-relay-delayed-offer").await;
    let mut call = s.alice.invite(&s.bob).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert!(uas.request().body().is_empty(), "an offerless INVITE");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack_with_stating(Some(OFFER), ACK_LINES).await;

    let ack = s.bob.receive("ACK").await;
    assert_eq!(ack.request().body(), OFFER.as_bytes(), "the answer rides the ACK");
    assert_eq!(lines(ack.request(), "User-Agent"), ["AliceSoft/1.0"]);
    assert_eq!(lines(ack.request(), "X-Vendor-Note"), ["acked"]);

    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
