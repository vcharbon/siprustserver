//! A deployment's static relay policy across every relay the back-to-back UA
//! performs: a provisional, the final, a re-INVITE and its answer each way, and
//! the teardown each way. An entry removes its header from the message it names
//! and nowhere else; the originated INVITE is never a relay and keeps it.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallTreatment, RejectDecision, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use sip_message::generators::{InDialogMethod, MessageClass, RelayDirection, RelayPolicy};
use sip_message::header::HeaderName;
use sip_message::method::Method;
use sip_message::{SipRequest, SipResponse};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

const CALLER_PAI: &str = "<sip:+15550001@op.example>";
const CALLEE_PAI: &str = "<sip:+15550002@op.example>";
const PAI: &str = "P-Asserted-Identity";
/// An extension header no entry names: it rides beside every removal.
const NOTE: &str = "X-Relay-Note";

fn req_lines(req: &SipRequest, name: &str) -> Vec<String> {
    req.raw(HeaderName::from(name)).map(str::to_string).collect()
}

fn resp_lines(resp: &SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// The asserted identity stays behind on every 2xx to an INVITE, on every
/// relayed re-INVITE, and on a BYE toward the called party.
fn identity_policy() -> RelayPolicy {
    RelayPolicy::transparent()
        .dropping(PAI, MessageClass::Response { class: 2, method: Method::Invite }, None)
        .dropping(PAI, MessageClass::Request(Method::Invite), None)
        .dropping(PAI, MessageClass::Request(Method::Bye), Some(RelayDirection::TowardCallee))
}

async fn scene(name: &str, policy: RelayPolicy) -> B2buaScene {
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(move |c| {
            c.privacy_service = false;
            c.relay_policy = policy;
        })
    })
    .await
}

/// What one call shows of the asserted identity and of the unnamed note on the
/// far side of each relay, every message carrying both. `bob_hangs_up` picks
/// who sends the BYE. Each entry is (message, identity lines, note lines).
async fn seen_across_a_call(
    s: &B2buaScene,
    bob_hangs_up: bool,
) -> Vec<(&'static str, Vec<String>, Vec<String>)> {
    let mut seen = Vec::new();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header(PAI, CALLER_PAI)
        .with_header(NOTE, "invite")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    seen.push(("INVITE", req_lines(uas.request(), PAI), req_lines(uas.request(), NOTE)));

    uas.respond(180, "Ringing").with_header(PAI, CALLEE_PAI).with_header(NOTE, "180").await;
    let ringing = call.expect(180).await;
    seen.push(("180", resp_lines(&ringing, PAI), resp_lines(&ringing, NOTE)));

    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header(PAI, CALLEE_PAI)
        .with_header(NOTE, "200")
        .await;
    let ok = call.expect(200).await;
    seen.push(("200", resp_lines(&ok, PAI), resp_lines(&ok, NOTE)));
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut reinvite = bob_dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .with_header(PAI, CALLEE_PAI)
        .with_header(NOTE, "re-INVITE")
        .send()
        .await;
    let mut alice_uas = s.alice.receive("INVITE").await;
    seen.push((
        "re-INVITE",
        req_lines(alice_uas.request(), PAI),
        req_lines(alice_uas.request(), NOTE),
    ));
    alice_uas
        .respond(200, "OK")
        .with_sdp(REANSWER)
        .with_header(PAI, CALLER_PAI)
        .with_header(NOTE, "re-INVITE 200")
        .await;
    let reinvite_ok = reinvite.expect(200).await;
    seen.push(("re-INVITE 200", resp_lines(&reinvite_ok, PAI), resp_lines(&reinvite_ok, NOTE)));
    bob_dialog.ack(None).await;
    s.alice.receive("ACK").await;

    if bob_hangs_up {
        let mut bye = bob_dialog
            .send_request(InDialogMethod::Bye)
            .with_header(PAI, CALLEE_PAI)
            .with_header(NOTE, "BYE")
            .send()
            .await;
        let mut alice_bye = s.alice.receive("BYE").await;
        seen.push((
            "BYE",
            req_lines(alice_bye.request(), PAI),
            req_lines(alice_bye.request(), NOTE),
        ));
        alice_bye.respond(200, "OK").await;
        bye.expect(200).await;
    } else {
        let mut bye = alice_dialog
            .send_request(InDialogMethod::Bye)
            .with_header(PAI, CALLER_PAI)
            .with_header(NOTE, "BYE")
            .send()
            .await;
        let mut bob_bye = s.bob.receive("BYE").await;
        seen.push(("BYE", req_lines(bob_bye.request(), PAI), req_lines(bob_bye.request(), NOTE)));
        bob_bye.respond(200, "OK").await;
        bye.expect(200).await;
    }
    seen
}

/// The identity the sender of `message` stated, in a call where alice hangs
/// up unless `bob_hangs_up`.
fn sent_identity(message: &str, bob_hangs_up: bool) -> &'static str {
    match message {
        "INVITE" | "re-INVITE 200" => CALLER_PAI,
        "BYE" if !bob_hangs_up => CALLER_PAI,
        _ => CALLEE_PAI,
    }
}

/// Each entry removes the identity from the message it names; the provisional
/// and the originated INVITE keep it, and the unnamed note rides everywhere.
#[tokio::test(start_paused = true)]
async fn each_entry_removes_its_header_from_the_message_it_names() {
    let s = scene("relay-policy-caller-hangs-up", identity_policy()).await;
    for (message, identity, note) in seen_across_a_call(&s, false).await {
        let kept = matches!(message, "INVITE" | "180");
        let expected: Vec<String> =
            if kept { vec![sent_identity(message, false).to_string()] } else { Vec::new() };
        assert_eq!(identity, expected, "{PAI} on the relayed {message}");
        assert_eq!(note.len(), 1, "{NOTE} rides on the relayed {message}");
    }
    let _report = s.finish().await;
}

/// An entry stating a direction holds only that way: the callee's BYE toward
/// the caller keeps the identity the caller's BYE toward the callee loses.
#[tokio::test(start_paused = true)]
async fn a_directed_entry_holds_only_in_its_direction() {
    let s = scene("relay-policy-callee-hangs-up", identity_policy()).await;
    let seen = seen_across_a_call(&s, true).await;
    let (_, identity, _) = seen.iter().find(|(m, _, _)| *m == "BYE").expect("a BYE");
    assert_eq!(identity, &[CALLEE_PAI], "{PAI} rides on the BYE toward the caller");
    let _report = s.finish().await;
}

/// The default policy is transparent: every relay carries the identity.
#[tokio::test(start_paused = true)]
async fn the_default_policy_relays_every_header() {
    let s = scene("relay-policy-transparent", RelayPolicy::transparent()).await;
    for (message, identity, _) in seen_across_a_call(&s, false).await {
        assert_eq!(identity, [sent_identity(message, false)], "{PAI} on the relayed {message}");
    }
    let _report = s.finish().await;
}

/// A scene whose route asks the decision about a callee failure and answers
/// every consult with a 603 of its own.
async fn consulting_scene(name: &str, policy: RelayPolicy) -> B2buaScene {
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.callback_context = Some("consult".into());
                    CallTreatment::Route(r)
                })
                .on_failure(|_| {
                    CallTreatment::Reject(RejectDecision {
                        reject_code: 603,
                        reject_reason: Some("Decline".into()),
                        update_headers: None,
                        service_ext: Default::default(),
                        label: None,
                    })
                })
                .build(),
        ))
        .tune(move |c| {
            c.worker_allowed_target_suffixes = vec!["*".into()];
            c.relay_policy = policy;
        })
    })
    .await
}

/// The final a decision authors after the consult carries the callee's
/// failure lines under the policy read for the final it is: a 603, whatever
/// status the callee refused with.
#[tokio::test(start_paused = true)]
async fn the_final_after_a_consult_reads_the_policy_for_its_own_status() {
    let decline = MessageClass::Response { class: 6, method: Method::Invite };
    let busy = MessageClass::Response { class: 4, method: Method::Invite };
    for (class, rides) in [(decline, false), (busy, true)] {
        let policy = RelayPolicy::transparent().dropping(NOTE, class.clone(), None);
        let s = consulting_scene("relay-policy-consult", policy).await;
        let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
        let mut uas = s.bob.receive("INVITE").await;
        uas.respond(486, "Busy Here")
            .with_header(NOTE, "busy")
            .with_header("X-Other", "kept")
            .await;
        s.bob.receive("ACK").await;
        let declined = call.expect(603).await;
        let expected: Vec<String> = if rides { vec!["busy".into()] } else { Vec::new() };
        assert_eq!(resp_lines(&declined, NOTE), expected, "{NOTE} under an entry for {class:?}");
        assert_eq!(resp_lines(&declined, "X-Other"), ["kept"], "the unnamed line rides");
        let _report = s.finish().await;
    }
}

/// A failing final relayed straight through (no consult) reads the policy for
/// that final.
#[tokio::test(start_paused = true)]
async fn a_relayed_failing_final_reads_the_policy() {
    let policy = RelayPolicy::transparent().dropping(
        NOTE,
        MessageClass::Response { class: 4, method: Method::Invite },
        Some(RelayDirection::TowardCaller),
    );
    let s = scene("relay-policy-486", policy).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(486, "Busy Here").with_header(NOTE, "busy").with_header("X-Other", "kept").await;
    s.bob.receive("ACK").await;
    let busy = call.expect(486).await;
    assert_eq!(resp_lines(&busy, NOTE), Vec::<String>::new(), "{NOTE} stays behind");
    assert_eq!(resp_lines(&busy, "X-Other"), ["kept"], "the unnamed line rides");
    let _report = s.finish().await;
}

/// The CANCEL minted for the caller's CANCEL reads the policy for a CANCEL
/// toward the callee.
#[tokio::test(start_paused = true)]
async fn the_cancel_toward_the_callee_reads_the_policy() {
    let policy = RelayPolicy::transparent().dropping(
        NOTE,
        MessageClass::Request(Method::Cancel),
        Some(RelayDirection::TowardCallee),
    );
    let s = scene("relay-policy-cancel", policy).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cxl = call.cancel_stating(&[(NOTE, "cancel"), ("X-Other", "kept")]).await;
    let mut b_cxl = s.bob.receive("CANCEL").await;
    assert_eq!(req_lines(b_cxl.request(), NOTE), Vec::<String>::new(), "{NOTE} stays behind");
    assert_eq!(req_lines(b_cxl.request(), "X-Other"), ["kept"], "the unnamed line rides");
    b_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    cxl.expect(200).await;
    call.expect(487).await;
    let _report = s.finish().await;
}
