//! Which re-INVITE 2xx gets a body on its ACK (RFC 3261 §13.2.2.4 / §13.2.1):
//! the answer rides the ACK ONLY when the re-INVITE was bodyless and the 2xx
//! carried the offer; a re-INVITE that carried the offer itself closes its
//! round on the response and takes a bodyless ACK. An explicit
//! `ExpectResponse{ack_body}` overrides both.

use std::collections::HashMap;
use std::time::Duration;

use sip_message::{EmitOpts, MessageTemplate, Method, TemplateHeader};

use super::testkit::*;
use crate::actor::*;
use crate::{Harness, OFFER_SDP};

/// Alice's answer to a delayed offer — the same session as her `OFFER_SDP`
/// (one `o=` username + sess-id, a raised sess-version), audio over RTP/AVP.
const ALICE_ANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Bob's answer to alice's offer.
const BOB_ANSWER: &str = "v=0\r\no=bob 7 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Bob's DELAYED offer — the one that rides his 2xx to a bodyless re-INVITE.
const BOB_OFFER: &str = "v=0\r\no=bob 7 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The establishing INVITE goals every case here shares: INVITE with the offer,
/// then consume the 180 and the 200 in order so the re-INVITE's own final
/// aligns on the goal that follows.
fn establish_goals() -> Vec<Goal> {
    vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::None, expect(180, None)),
        Goal::new(Barrier::None, expect(200, None)),
    ]
}

fn expect(status: u16, ack_body: Option<Vec<u8>>) -> GoalStep {
    GoalStep::ExpectResponse {
        status,
        cseq_method: None,
        body: BodyExpect::Any,
        early: None,
        ack_body,
        matcher: None,
    }
}

fn alice_confirmed() -> Barrier {
    Barrier::pred("alice_confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed))
}

/// A re-INVITE template carrying alice's own offer.
fn offer_reinvite() -> MessageTemplate {
    MessageTemplate::request(
        Method::Invite,
        vec![TemplateHeader::frozen("Content-Type", "application/sdp")],
        OFFER_SDP.as_bytes().to_vec(),
    )
}

/// The caller's re-INVITE carried the offer and bob's 200 the answer — the
/// round is complete, so the ACK carries NO session description.
#[tokio::test(start_paused = true)]
async fn ack_to_an_offer_carrying_reinvite_is_bodyless() {
    let h = Harness::new("actor-reinvite-ack-bodyless").describe(
        "offer on the re-INVITE, answer on the 200: the ACK closing that \
         round carries no body (RFC 3261 §13.2.2.4)",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bob_srv = bob.clone();
    let server = tokio::spawn(async move {
        let bob = bob_srv;
        let mut inv = bob.try_receive("INVITE").await.unwrap();
        inv.respond(180, "Ringing").try_send().await.unwrap();
        inv.respond(200, "OK").with_sdp(BOB_ANSWER).try_send().await.unwrap();
        bob.try_receive("ACK").await.unwrap();
        let mut re = bob.try_receive("INVITE").await.unwrap();
        assert_eq!(re.request().body(), OFFER_SDP.as_bytes(), "the re-INVITE carries the offer");
        re.respond(200, "OK").with_sdp(BOB_ANSWER).try_send().await.unwrap();
        let ack = bob.try_receive("ACK").await.unwrap();
        assert!(
            ack.request().body().is_empty(),
            "the ACK to an already-answered offer carries no body, got {:?}",
            String::from_utf8_lossy(ack.request().body()),
        );
        bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
    });

    let mut goals = establish_goals();
    goals.extend([
        Goal::new(
            alice_confirmed(),
            GoalStep::RequestTemplate {
                template: offer_reinvite(),
                opts: EmitOpts::default(),
                early: false,
            },
        ),
        Goal::new(Barrier::None, expect(200, None)),
        Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(100)),
    ]);
    let call = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals)],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the offer-carrying re-INVITE must settle OK, got {verdict:?}");
    server.await.unwrap();
    h.finish().await;
}

/// The genuine delayed-offer form (RFC 3264 §4): the re-INVITE is bodyless, the
/// 2xx carries bob's offer, so the ACK carries alice's answer.
#[tokio::test(start_paused = true)]
async fn ack_to_a_delayed_offer_reinvite_carries_the_answer() {
    let h = Harness::new("actor-reinvite-ack-delayed-offer").describe(
        "bodyless re-INVITE, offer on the 200: the ACK carries the answer \
         (RFC 3264 §4)",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bob_srv = bob.clone();
    let server = tokio::spawn(async move {
        let bob = bob_srv;
        let mut inv = bob.try_receive("INVITE").await.unwrap();
        inv.respond(180, "Ringing").try_send().await.unwrap();
        inv.respond(200, "OK").with_sdp(BOB_ANSWER).try_send().await.unwrap();
        bob.try_receive("ACK").await.unwrap();
        let mut re = bob.try_receive("INVITE").await.unwrap();
        assert!(re.request().body().is_empty(), "the re-INVITE is bodyless (delayed offer)");
        re.respond(200, "OK").with_sdp(BOB_OFFER).try_send().await.unwrap();
        let ack = bob.try_receive("ACK").await.unwrap();
        assert_eq!(
            ack.request().body(),
            ALICE_ANSWER.as_bytes(),
            "the ACK closes the delayed-offer round with alice's answer",
        );
        bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
    });

    let mut goals = establish_goals();
    goals.extend([
        Goal::new(alice_confirmed(), GoalStep::Reinvite),
        Goal::new(Barrier::None, expect(200, None)),
        Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(100)),
    ]);
    let mut spec = caller_spec("alice", &alice, ("bob", bob.clone()), goals);
    spec.media = MediaState::full(OFFER_SDP, ALICE_ANSWER);
    let call = CallPlan {
        actors: vec![spec],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the delayed-offer re-INVITE must settle OK, got {verdict:?}");
    server.await.unwrap();
    h.finish().await;
}

/// An explicit `ExpectResponse{ack_body}` outranks the round's own verdict: it
/// rides the ACK even where the exchange owes no body (a capture replaying an
/// endpoint that ACKs with SDP).
#[test]
fn an_explicit_ack_body_overrides_the_bodyless_round() {
    let mut cache: HashMap<u32, Option<String>> = HashMap::new();
    let override_goal = expect(200, Some(b"captured-ack-sdp".to_vec()));
    assert_eq!(
        crate::actor::response::resolve_ack_body(&mut cache, Some(&override_goal), None, 7),
        Some("captured-ack-sdp".to_string()),
    );
}
