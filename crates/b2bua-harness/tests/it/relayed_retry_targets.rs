//! A relayed 3xx carries the peer's redirect targets (RFC 3261 §8.1.3.4,
//! §21.3) and a relayed 485 its alternates (§21.4.22): the peer's Contact set,
//! never the B2BUA's own. A final the B2BUA or its decision authors after a
//! peer's 3xx / 485 carries none of those targets, and a redirect decision
//! that is not a 3xx is refused.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallFailureResponse, CallTreatment, HeaderUpdate, NewCallResponse, RedirectContact,
    RedirectDecision, RejectDecision, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;
use sip_message::header::Contact;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendonly\r\n";

const CAROL: &str = "<sip:carol@10.0.0.9>";
const ALTERNATES: &str = "<sip:carol@10.0.0.9>, <sip:dave@10.0.0.10>";

/// The Contact URIs `resp` carries, as written.
fn contact_uris(resp: &SipResponse) -> Vec<String> {
    resp.list::<Contact>()
        .expect("readable Contact list")
        .iter()
        .map(|c| c.uri().to_string())
        .collect()
}

/// `resp` carries every one of `targets` and no Contact naming the B2BUA.
fn assert_peer_targets(resp: &SipResponse, targets: &[&str], b2bua: SocketAddr) {
    let contacts = resp.list::<Contact>().expect("readable Contact list");
    let uris = contact_uris(resp);
    for t in targets {
        assert!(uris.iter().any(|u| u == t), "{} carries {t}: {uris:?}", resp.status());
    }
    for c in &contacts {
        assert!(c.uri().param("leg").is_none(), "no stack Contact on {}: {uris:?}", resp.status());
        assert_ne!(
            (c.uri().host(), c.uri().port()),
            (b2bua.ip().to_string().as_str(), Some(b2bua.port())),
            "no Contact naming the B2BUA on {}: {uris:?}",
            resp.status()
        );
    }
}

/// No callback context: Bob's 302 to the initial INVITE relays to Alice with
/// his redirect target.
#[tokio::test(start_paused = true)]
async fn a_relayed_302_to_the_initial_invite_carries_the_peer_targets() {
    let s = B2buaScene::new("relayed-302-initial-invite").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(302, "Moved Temporarily").with_header("Contact", CAROL).await;
    s.bob.receive("ACK").await;
    let redirected = call.expect(302).await;
    assert_peer_targets(&redirected, &["sip:carol@10.0.0.9"], s.b2bua.addr);

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// A consultable call whose failure decision lets the peer's final stand:
/// Bob's 302 reaches Alice with his redirect target.
#[tokio::test(start_paused = true)]
async fn a_302_the_failure_decision_lets_stand_carries_the_peer_targets() {
    let h = Harness::new("relayed-302-failure-terminate");
    let alice = h.agent("alice", "127.0.0.1:7121").await;
    let bob = h.agent("bob", "127.0.0.1:7122").await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut r = route_to("127.0.0.1", 7122);
            r.callback_context = Some("redirect-stands".into());
            NewCallResponse::Route(r)
        })
        .on_failure(|_req| CallFailureResponse::Relay { label: None })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:7123").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(302, "Moved Temporarily").with_header("Contact", CAROL).await;
    bob.receive("ACK").await;
    let redirected = call.expect(302).await;
    assert_peer_targets(&redirected, &["sip:carol@10.0.0.9"], b2bua.addr);

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A consultable call whose failure decision authors its own 480 after Bob's
/// 302: the 480 speaks for the B2BUA and carries none of Bob's targets.
#[tokio::test(start_paused = true)]
async fn a_decision_final_after_a_peer_302_carries_none_of_its_targets() {
    let h = Harness::new("decision-final-after-302");
    let alice = h.agent("alice", "127.0.0.1:7124").await;
    let bob = h.agent("bob", "127.0.0.1:7125").await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut r = route_to("127.0.0.1", 7125);
            r.callback_context = Some("redirect-refused".into());
            NewCallResponse::Route(r)
        })
        .on_failure(|_req| {
            CallTreatment::Reject(RejectDecision {
                reject_code: 480,
                reject_reason: Some("Temporarily Unavailable".into()),
                update_headers: None,
                service_ext: Default::default(),
                label: None,
            })
        })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:7126").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(302, "Moved Temporarily").with_header("Contact", CAROL).await;
    bob.receive("ACK").await;
    let refused = call.expect(480).await;
    assert!(
        contact_uris(&refused).is_empty(),
        "no Contact on the 480: {:?}",
        contact_uris(&refused)
    );

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// In dialog: Bob answers Alice's re-INVITE 302 and her UPDATE 485; both reach
/// her with his targets, and the call stays up (RFC 3261 §14.1).
#[tokio::test(start_paused = true)]
async fn relayed_in_dialog_302_and_485_carry_the_peer_targets() {
    let s = B2buaScene::new("relayed-in-dialog-302-485").await;
    let mut dialog = s.establish().await;

    let mut reinvite = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(302, "Moved Temporarily").with_header("Contact", CAROL).await;
    let redirected = reinvite.expect(302).await;
    assert_peer_targets(&redirected, &["sip:carol@10.0.0.9"], s.b2bua.addr);
    s.alice.drain().await;
    s.bob.drain().await;

    let mut update = dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    s.bob
        .receive("UPDATE")
        .await
        .respond(485, "Ambiguous")
        .with_header("Contact", ALTERNATES)
        .await;
    let ambiguous = update.expect(485).await;
    assert_peer_targets(&ambiguous, &["sip:carol@10.0.0.9", "sip:dave@10.0.0.10"], s.b2bua.addr);

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

fn reject(code: u16, reason: &str) -> CallTreatment {
    CallTreatment::Reject(RejectDecision {
        reject_code: code,
        reject_reason: Some(reason.into()),
        update_headers: None,
        service_ext: Default::default(),
        label: None,
    })
}

fn redirect(code: u16, target: &str) -> CallTreatment {
    CallTreatment::Redirect(RedirectDecision {
        code,
        reason: None,
        contacts: vec![RedirectContact { uri: target.into(), q: None }],
        update_headers: None,
        service_ext: Default::default(),
        label: None,
    })
}

/// Ports for one consultable call: alice, bob, the B2BUA.
struct Ports(u16, u16, u16);

/// A consultable call: Bob refuses with `peer_status` and the `peer_headers`
/// lines, the failure decision answers `decision`, and Alice's final is
/// returned once the call is reaped.
async fn final_after_peer_refusal(
    name: &str,
    Ports(a, b, sut): Ports,
    (peer_status, peer_reason, peer_headers): (u16, &str, &[(&str, &str)]),
    decision: CallTreatment,
    expected: u16,
) -> SipResponse {
    let h = Harness::new(name);
    let alice = h.agent("alice", &format!("127.0.0.1:{a}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{b}")).await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(move |_req| {
            let mut r = route_to("127.0.0.1", b);
            r.callback_context = Some("consult".into());
            NewCallResponse::Route(r)
        })
        .on_failure(move |_req| decision.clone())
        .build();
    let b2bua =
        B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", &format!("127.0.0.1:{sut}")).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    let mut refusal = uas.respond(peer_status, peer_reason);
    for (name, value) in peer_headers {
        refusal = refusal.with_header(name, value);
    }
    refusal.await;
    bob.receive("ACK").await;
    let answered = call.expect(expected).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
    answered
}

/// The decision authors a 485 after Bob's 302: it lists no alternates of its
/// own, so the 485 carries no Contact, Bob's target least of all.
#[tokio::test(start_paused = true)]
async fn a_decision_485_after_a_peer_302_carries_none_of_its_targets() {
    let ambiguous = final_after_peer_refusal(
        "decision-485-after-302",
        Ports(7131, 7132, 7133),
        (302, "Moved Temporarily", &[("Contact", CAROL)]),
        reject(485, "Ambiguous"),
        485,
    )
    .await;
    assert!(contact_uris(&ambiguous).is_empty(), "{:?}", contact_uris(&ambiguous));
}

/// The decision authors a 485 after Bob's own 485: the alternates are Bob's,
/// and the decision is now the author (§21.4.22), so none of them ride.
#[tokio::test(start_paused = true)]
async fn a_decision_485_after_a_peer_485_carries_none_of_its_alternates() {
    let ambiguous = final_after_peer_refusal(
        "decision-485-after-485",
        Ports(7134, 7135, 7136),
        (485, "Ambiguous", &[("Contact", ALTERNATES)]),
        reject(485, "Ambiguous"),
        485,
    )
    .await;
    assert!(contact_uris(&ambiguous).is_empty(), "{:?}", contact_uris(&ambiguous));
}

/// A failover redirect stating a non-3xx code is not a redirect: it is
/// refused with the plain server error, which speaks only for itself. It
/// carries neither the redirect's target nor its `update_headers`, and nothing
/// of Bob's refusal: no Contact, `Warning` or `Retry-After`.
#[tokio::test(start_paused = true)]
async fn a_failover_redirect_that_is_not_a_3xx_is_refused() {
    let CallTreatment::Redirect(mut refused_redirect) = redirect(485, "sip:x@10.0.0.11") else {
        unreachable!("a redirect treatment")
    };
    refused_redirect.update_headers =
        Some([("X-Decision".to_string(), HeaderUpdate::line("redirect"))].into_iter().collect());
    let refused = final_after_peer_refusal(
        "failover-redirect-485",
        Ports(7137, 7138, 7139),
        (486, "Busy Here", &[("Warning", "399 bob.example \"busy\""), ("Retry-After", "60")]),
        CallTreatment::Redirect(refused_redirect),
        500,
    )
    .await;
    assert!(contact_uris(&refused).is_empty(), "{:?}", contact_uris(&refused));
    for name in ["Warning", "Retry-After", "X-Decision"] {
        assert!(
            refused.raw(sip_message::header::HeaderName::from(name)).next().is_none(),
            "no {name} on the refused redirect's 500"
        );
    }
}

/// A new-call redirect stating a non-3xx code is refused the same way.
#[tokio::test(start_paused = true)]
async fn an_initial_redirect_that_is_not_a_3xx_is_refused() {
    let h = Harness::new("initial-redirect-485");
    let alice = h.agent("alice", "127.0.0.1:7140").await;
    let bob = h.agent("bob", "127.0.0.1:7141").await;
    let engine =
        ScriptedDecisionEngine::builder().fallback(|_req| redirect(485, "sip:x@10.0.0.11")).build();
    let b2bua = B2buaSut::builder(Arc::new(engine)).start(&h, "b2bua", "127.0.0.1:7142").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let refused = call.expect(500).await;
    assert!(contact_uris(&refused).is_empty(), "{:?}", contact_uris(&refused));

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A relayed 302 carries only the targets that read as Contact entries, entry
/// by entry: a `Contact: *` (meaningful only in a REGISTER, §10.2.2) and an
/// unreadable entry are dropped, as a decision's unreadable target is refused,
/// while a readable entry beside either still rides with its parameters.
#[tokio::test(start_paused = true)]
async fn a_relayed_302_drops_a_wildcard_and_an_unreadable_contact() {
    let s = B2buaScene::new("relayed-302-wildcard-unreadable").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(302, "Moved Temporarily")
        .with_header("Contact", "*")
        .with_header("Contact", "<sip:@>")
        .with_header("Contact", "<sip:carol@10.0.0.9>, <sip:@>")
        .with_header("Contact", "<sip:dave@10.0.0.10>;q=0.5, <sip:bob@10.0.0.2:99999>")
        .with_header("Contact", "*, <sip:erin@10.0.0.11>")
        .await;
    s.bob.receive("ACK").await;
    let redirected = call.expect(302).await;
    let raw: Vec<&str> = redirected.raw(sip_message::header::HeaderName::Contact).collect();
    assert!(!raw.iter().any(|v| v.contains('*')), "no wildcard Contact: {raw:?}");
    assert!(!raw.iter().any(|v| v.contains("sip:@")), "no unreadable Contact: {raw:?}");
    assert!(!raw.iter().any(|v| v.contains("99999")), "no unreadable Contact: {raw:?}");
    assert!(raw.contains(&"<sip:dave@10.0.0.10>;q=0.5"), "q rides with dave: {raw:?}");
    assert_peer_targets(
        &redirected,
        &["sip:carol@10.0.0.9", "sip:dave@10.0.0.10", "sip:erin@10.0.0.11"],
        s.b2bua.addr,
    );

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}
