//! A request requiring an option tag the call withholds from the peer that
//! would serve it: the stack understands the tag but cannot meet the
//! requirement on that call, so it answers 420 Bad Extension naming it in
//! `Unsupported` (RFC 3261 §8.2.2.3), never relays the request with the
//! requirement silently dropped. The session timer (RFC 4028 §7.1) is served
//! end to end by the peer; a call that withholds `timer` from that peer (a
//! strategy's withhold, a declared one, a `Supported` without it, a leg
//! dialled outside the timer) serves no one's `Require: timer`. Each case ends
//! with its CDR.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{route_to, route_to_with_18x};
use b2bua::decision::{NewCallResponse, RouteDecision, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, B2buaSutBuilder};
use call::features::{AdvertiseCapabilitiesFeature, AdvertisedCapabilities, RelayFirst18xStrategy};
use call::header_update::HeaderUpdate;
use call::TerminationCause;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, Dialog, Harness, InDialogRequest, Invite};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::SipResponse;

/// Past a request's two hops through the stack to the peer and short of its
/// first retransmission (RFC 3261 §17.1.2.2, T1).
const RELAY_WINDOW: Duration = Duration::from_millis(300);

const ALICE_REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The `Unsupported` tags `resp` lists, one per entry, in order.
fn unsupported(resp: &SipResponse) -> Vec<String> {
    resp.raw(HeaderName::Unsupported)
        .flat_map(|line| line.split(','))
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// How the scripted decision routes every call to bob.
#[derive(Clone, Copy)]
struct Routing {
    strategy: Option<RelayFirst18xStrategy>,
    /// The call-scoped declaration `features.withhold_option_tags`.
    withheld: &'static [&'static str],
    /// The declared `Supported` toward the originated face.
    supported_toward_bob: Option<&'static [&'static str]>,
    /// The `Supported` line the route states on bob's INVITE
    /// (`update_headers`), which outranks the face's declaration.
    supported_stated: Option<&'static [&'static str]>,
}

impl Routing {
    const PLAIN: Routing = Routing {
        strategy: None,
        withheld: &[],
        supported_toward_bob: None,
        supported_stated: None,
    };

    fn under(strategy: RelayFirst18xStrategy) -> Self {
        Routing { strategy: Some(strategy), ..Self::PLAIN }
    }

    fn withholding(withheld: &'static [&'static str]) -> Self {
        Routing { withheld, ..Self::PLAIN }
    }

    fn builder(self, bob_port: u16) -> B2buaSutBuilder {
        let decide = move |_req: &_| {
            let mut r = match self.strategy {
                Some(strategy) => route_to_with_18x("127.0.0.1", bob_port, strategy),
                None => route_to("127.0.0.1", bob_port),
            };
            if !self.withheld.is_empty() {
                r.features.withhold_option_tags =
                    Some(self.withheld.iter().map(|t| t.to_string()).collect());
            }
            if let Some(supported) = self.supported_toward_bob {
                r.features.advertise_capabilities = Some(AdvertiseCapabilitiesFeature {
                    toward_originator: None,
                    toward_originated: Some(AdvertisedCapabilities {
                        allow: None,
                        supported: Some(supported.iter().map(|t| t.to_string()).collect()),
                    }),
                });
            }
            if let Some(supported) = self.supported_stated {
                let line = HeaderUpdate::line(supported.join(", "));
                r.update_headers = Some([("Supported".to_string(), line)].into_iter().collect());
            }
            NewCallResponse::Route(r)
        };
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::builder().fallback(decide).build()))
    }
}

/// `invite` stating the session timer RFC 4028 §7.1 asks of a UAC that
/// requires it: `Supported: timer` and an interval.
fn with_timer_offer(invite: Invite<'_>) -> Invite<'_> {
    invite.with_header("Supported", "timer").with_header("Session-Expires", "1800;refresher=uac")
}

/// alice's initial INVITE requiring `require`, offering where `offer`, under
/// `routing`: bob is sent nothing, she is answered 420, and the call has its
/// one CDR, cause `Admission`. Returns the tags the 420 lists.
async fn refused_at_setup(name: &str, routing: Routing, require: &str, offer: bool) -> Vec<String> {
    let s = B2buaScene::with_b2bua(name, |bob_port| routing.builder(bob_port)).await;
    let invite = s.alice.invite(&s.bob).with_header("Require", require);
    let invite = if require.contains("timer") { with_timer_offer(invite) } else { invite };
    let invite = if offer { invite.with_sdp(OFFER_SDP) } else { invite };
    let mut call = invite.through(s.b2bua.addr).send().await;
    s.h.advance(RELAY_WINDOW).await;
    let dialled = s.bob.take_queued().await;
    assert!(dialled.is_none(), "nobody is dialled, bob got {dialled:?}");
    let refusal = call.expect(420).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let cdrs = s.b2bua.cdr_records();
    let cdr = cdrs.iter().find(|r| r.a_leg.call_id == call.call_id()).expect("its CDR");
    assert!(cdr.b_legs.is_empty(), "no b-leg: {cdr:?}");
    assert_eq!(cdr.termination.as_ref().map(|t| t.cause), Some(TerminationCause::Admission));
    let _report = s.finish().await;
    unsupported(&refusal)
}

#[tokio::test(start_paused = true)]
async fn the_promotion_with_an_offer_refuses_a_required_timer() {
    let routing = Routing::under(RelayFirst18xStrategy::PromotePemTo200);
    let listed = refused_at_setup("withheld-timer-promote-offer", routing, "timer", true).await;
    assert_eq!(listed, ["timer"]);
}

/// With no offer the promotion withholds both: the 420 lists every required
/// tag the call withholds, in the order the request lists them.
#[tokio::test(start_paused = true)]
async fn the_promotion_with_no_offer_lists_every_withheld_tag_required() {
    let routing = Routing::under(RelayFirst18xStrategy::PromotePemTo200);
    let listed =
        refused_at_setup("withheld-timer-promote-no-offer", routing, "100rel, timer", false).await;
    assert_eq!(listed, ["100rel", "timer"]);
}

#[tokio::test(start_paused = true)]
async fn a_declared_withhold_refuses_a_required_timer() {
    let routing = Routing::withholding(&["timer"]);
    let listed = refused_at_setup("withheld-timer-declared", routing, "timer", true).await;
    assert_eq!(listed, ["timer"]);
}

/// A route whose dial a service defers (an announcement first, the
/// destination after it) withholds `timer` as any route does: the caller
/// requiring it is refused before the service dials anyone, the MRF included.
#[tokio::test(start_paused = true)]
async fn a_deferred_route_declaring_its_withhold_refuses_a_required_timer() {
    let declare: fn(&mut RouteDecision) = |r| {
        r.features.withhold_option_tags = Some(vec!["timer".into()]);
    };
    refused_before_the_service_dials("withheld-timer-deferred-route", 5670, declare).await;
}

/// A deferred route declaring bob's face a `Supported` without `timer`: no
/// INVITE is built yet, and the declaration alone keeps the timer off his leg.
#[tokio::test(start_paused = true)]
async fn a_deferred_route_declaring_a_face_without_timer_refuses_a_required_timer() {
    let declare: fn(&mut RouteDecision) = |r| {
        r.features.advertise_capabilities = Some(AdvertiseCapabilitiesFeature {
            toward_originator: None,
            toward_originated: Some(AdvertisedCapabilities {
                allow: None,
                supported: Some(vec!["100rel".into()]),
            }),
        });
    };
    refused_before_the_service_dials("withheld-timer-deferred-face", 5671, declare).await;
}

/// alice's INVITE requiring `timer` on a route `declare` keeps the timer off
/// bob's leg, whose dial an announcement service on `mrf_port` defers: she is
/// answered 420 naming it, and nobody is dialled.
async fn refused_before_the_service_dials(
    name: &str,
    mrf_port: u16,
    declare: fn(&mut RouteDecision),
) {
    let s = B2buaScene::with_b2bua(name, |bob_port| {
        let decide = move |_req: &_| {
            let mut r = route_to("127.0.0.1", bob_port);
            declare(&mut r);
            r.service_ext.insert(
                "announcement".into(),
                serde_json::json!({
                    "clip_id": "intro-001",
                    "mrf_host": "127.0.0.1",
                    "mrf_port": mrf_port,
                    "dest_host": "127.0.0.1",
                    "dest_port": bob_port,
                    "defer_routing": true,
                }),
            );
            NewCallResponse::Route(r)
        };
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::builder().fallback(decide).build()))
            .services(vec![announcement::service()])
    })
    .await;
    let mrf = s.h.agent("mrf", &format!("127.0.0.1:{mrf_port}")).await;
    let invite = with_timer_offer(s.alice.invite(&s.bob)).with_header("Require", "timer");
    let mut call = invite.with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(RELAY_WINDOW).await;
    let dialled = (mrf.take_queued().await, s.bob.take_queued().await);
    assert!(matches!(dialled, (None, None)), "nobody is dialled: {dialled:?}");
    let refusal = call.expect(420).await;
    assert_eq!(unsupported(&refusal), ["timer"]);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let cdrs = s.b2bua.cdr_records();
    assert!(cdrs[0].b_legs.is_empty(), "no leg: {cdrs:?}");
    let _report = s.finish().await;
}

/// The callee's face declares a `Supported` without `timer`, which keeps the
/// session timer off his leg.
#[tokio::test(start_paused = true)]
async fn a_face_declared_without_timer_refuses_a_required_timer() {
    let routing = Routing { supported_toward_bob: Some(&["100rel"]), ..Routing::PLAIN };
    let listed = refused_at_setup("withheld-timer-face-setup", routing, "timer", true).await;
    assert_eq!(listed, ["timer"]);
}

/// The route states bob's INVITE a `Supported` without `timer`, which keeps
/// the session timer off his leg whatever the face declares.
#[tokio::test(start_paused = true)]
async fn a_route_stating_supported_without_timer_refuses_a_required_timer() {
    let routing = Routing { supported_stated: Some(&["100rel"]), ..Routing::PLAIN };
    let listed = refused_at_setup("withheld-timer-stated-setup", routing, "timer", true).await;
    assert_eq!(listed, ["timer"]);
}

/// The route states bob's INVITE a `Supported` naming `timer` over a face
/// declared without it: the INVITE offers the timer, so alice's requirement
/// rides to bob, who answers it.
#[tokio::test(start_paused = true)]
async fn a_route_stating_timer_over_a_face_declared_without_it_dials_the_requirement() {
    let routing = Routing {
        supported_toward_bob: Some(&["100rel"]),
        supported_stated: Some(&["100rel", "timer"]),
        ..Routing::PLAIN
    };
    let s = B2buaScene::with_b2bua("withheld-timer-stated-over-face", |bob_port| {
        routing.builder(bob_port)
    })
    .await;
    let invite = with_timer_offer(s.alice.invite(&s.bob)).with_header("Require", "timer");
    let mut call = invite.with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    let required: Vec<&str> = uas.request().raw(HeaderName::Require).collect();
    assert_eq!(required, ["timer"], "the requirement rides to bob");
    uas.respond(200, "OK")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .with_sdp(ANSWER_SDP)
        .await;
    call.expect(200).await;
    let mut alice = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut alice).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}

/// A confirmed call alice ↔ b2bua ↔ bob under `routing`.
async fn confirmed(name: &str, routing: Routing) -> (B2buaScene, Dialog, Dialog) {
    let s = B2buaScene::with_b2bua(name, |bob_port| routing.builder(bob_port)).await;
    let mut call: ClientInvite =
        s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let alice = call.ack().await;
    s.bob.receive("ACK").await;
    let bob = uas.dialog();
    (s, alice, bob)
}

/// alice's in-dialog `method` requiring `timer`, offering where `offer`, on a
/// call under `routing`: bob is sent nothing and she is answered 420 naming
/// it; a plain offer after it is relayed and answered; alice hangs up.
async fn timer_refused_in_dialog(
    name: &str,
    routing: Routing,
    method: InDialogMethod,
    offer: bool,
) {
    let (s, mut alice, _bob) = confirmed(name, routing).await;
    let method_name = method.as_str();
    let mut request = alice
        .send_request(method)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac");
    if offer {
        request = request.with_sdp(ALICE_REOFFER);
    }
    let mut txn = request.send().await;
    s.h.advance(RELAY_WINDOW).await;
    let relayed = s.bob.take_queued().await;
    assert!(relayed.is_none(), "the {method_name} is refused, not relayed; bob got {relayed:?}");
    let refusal = txn.expect(420).await;
    assert_eq!(unsupported(&refusal), ["timer"], "the 420 names the withheld tag");
    s.h.advance(RELAY_WINDOW).await;
    assert!(
        s.bob.take_queued().await.is_none(),
        "nothing of the refused {method_name} reaches bob"
    );
    if offer {
        let mut txn = alice.send_request(method).with_sdp(ALICE_REOFFER).send().await;
        s.bob.receive(method.as_str()).await.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        txn.expect(200).await;
        if matches!(method, InDialogMethod::Invite) {
            alice.ack(None).await;
            s.bob.receive("ACK").await;
        }
    }
    s.hangup(&mut alice).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one call, one CDR");
    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_update_requiring_timer_toward_a_leg_the_promotion_keeps_out_is_refused_420() {
    let routing = Routing::under(RelayFirst18xStrategy::PromotePemTo200);
    timer_refused_in_dialog("withheld-timer-update-promote", routing, InDialogMethod::Update, true)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_requiring_timer_toward_a_leg_the_promotion_keeps_out_is_refused_420() {
    let routing = Routing::under(RelayFirst18xStrategy::PromotePemTo200);
    timer_refused_in_dialog(
        "withheld-timer-reinvite-promote",
        routing,
        InDialogMethod::Invite,
        true,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_update_requiring_timer_on_a_call_declaring_its_withhold_is_refused_420() {
    let routing = Routing::withholding(&["timer"]);
    timer_refused_in_dialog(
        "withheld-timer-update-declared",
        routing,
        InDialogMethod::Update,
        false,
    )
    .await;
}

/// The call declares bob's face a `Supported` without `timer`: the request
/// toward him would lose the session timer, so its requirement is refused.
#[tokio::test(start_paused = true)]
async fn an_update_requiring_timer_toward_a_face_declared_without_it_is_refused_420() {
    let routing = Routing { supported_toward_bob: Some(&["100rel"]), ..Routing::PLAIN };
    timer_refused_in_dialog("withheld-timer-update-face", routing, InDialogMethod::Update, false)
        .await;
}

/// The withhold is toward the leg the call dialled: bob's UPDATE requiring
/// `timer` toward alice still reaches her with its requirement.
#[tokio::test(start_paused = true)]
async fn the_callees_update_requiring_timer_still_reaches_the_caller() {
    let routing = Routing::under(RelayFirst18xStrategy::PromotePemTo200);
    let (s, mut alice, mut bob) = confirmed("withheld-timer-callee-update", routing).await;
    let mut update = bob
        .send_request(InDialogMethod::Update)
        .with_header("Supported", "timer")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .send()
        .await;
    let mut at_alice = s.alice.receive("UPDATE").await;
    let required: Vec<&str> = at_alice.request().raw(HeaderName::Require).collect();
    assert_eq!(required, ["timer"], "the requirement rides to alice");
    at_alice
        .respond(200, "OK")
        .with_header("Require", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
        .await;
    update.expect(200).await;
    s.hangup(&mut alice).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// A transfer target is dialled outside the caller's session timer: its
/// answer to her INVITE never reached her. Once she is bridged to it, her
/// refresh requiring the timer is refused 420 naming it, and one offering the
/// timer reaches the target with no timer at all.
#[tokio::test(start_paused = true)]
async fn a_refresh_requiring_timer_toward_a_transfer_target_is_refused_420() {
    const CHARLIE_PORT: u16 = 5667;
    const CHARLIE_ANSWER: &str = "v=0\r\no=charlie 9 9 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
    let h = Harness::new("withheld-timer-transfer-target");
    let alice = h.agent("alice", "127.0.0.1:5903").await;
    let bob = h.agent("bob", "127.0.0.1:5913").await;
    let charlie = h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let b2bua = B2buaSut::route_all_with_refer("127.0.0.1", 5913)
        .start(&h, "b2bua", "127.0.0.1:5923")
        .await;

    let invite = with_timer_offer(alice.invite(&bob)).with_header("Require", "timer");
    let mut call = invite.with_sdp(OFFER_SDP).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();

    let api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    );
    bob_dialog
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &api)
        .send()
        .await
        .expect(202)
        .await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut charlie_uas = charlie.receive("INVITE").await;
    charlie_uas.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;
    bob.receive("NOTIFY").await.respond(200, "OK").await;
    charlie.receive("INVITE").await.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    charlie.receive("ACK").await;
    alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;

    let mut refused = refresh(&mut alice_dialog).with_header("Require", "timer").send().await;
    h.advance(RELAY_WINDOW).await;
    let relayed = charlie.take_queued().await;
    assert!(relayed.is_none(), "the refresh is refused, not relayed; charlie got {relayed:?}");
    let refusal = refused.expect(420).await;
    assert_eq!(unsupported(&refusal), ["timer"], "the 420 names the timer nobody serves");

    let mut offered = refresh(&mut alice_dialog).send().await;
    let mut charlie_refresh = charlie.receive("INVITE").await;
    for name in [HeaderName::SessionExpires, HeaderName::Require] {
        let lines: Vec<&str> = charlie_refresh.request().raw(name.clone()).collect();
        assert!(lines.is_empty(), "the refresh toward charlie carries no {name:?}: {lines:?}");
    }
    charlie_refresh.respond(200, "OK").with_sdp(CHARLIE_ANSWER).await;
    offered.expect(200).await;
    alice_dialog.ack(None).await;
    charlie.receive("ACK").await;

    let mut alice_bye = alice_dialog.bye().await;
    charlie.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call, one CDR");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// alice's session description once she is bridged to a transfer target.
const ALICE_REALIGN: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A refresh re-INVITE on `dialog`, offering the session as it stands and the
/// session timer with an interval (RFC 4028 §7.1).
fn refresh(dialog: &mut Dialog) -> InDialogRequest<'_> {
    dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(ALICE_REALIGN)
        .with_header("Supported", "timer")
        .with_header("Session-Expires", "1800;refresher=uac")
}
