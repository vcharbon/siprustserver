//! A message that names no call here, by the path it takes and what the peer
//! is owed.
//!
//! A request or response carrying this node's routing key (the Request-URI
//! `callRef` a B2BUA Contact stamps, the `cr` its Via stamps) resolves to a
//! call ref even once the call is gone and takes the orphan path. One with no
//! key and no index entry is unroutable. On either path a request other than
//! ACK is one a peer waits on and draws its RFC answer: 405 with `Allow` for a
//! method this node does not serve (RFC 3261 §8.2.1), 481 for any other
//! (§12.2.2, §15.1.2, §9.2), under a To-tag minted here when it came untagged
//! (§8.2.6.2). An ACK or a response is dropped. A client transaction released
//! from its call that later times out (Timer F) is an internal event: no peer
//! waits on it and it is no wire drop.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua::store::{StoreFaultPoint, StoreFaults};
use b2bua_harness::{establish, settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::{SipMessage, SipResponse};

/// What a confirmed dialog's caller side needs to write a request in it by hand.
struct DialogIds {
    call_id: String,
    from_uri: String,
    from_tag: String,
    to_uri: String,
    to_tag: String,
    /// The B2BUA's Contact: the remote target, carrying its `callRef`.
    remote_target: String,
}

impl DialogIds {
    fn of(answer: &SipResponse) -> Self {
        DialogIds {
            call_id: answer.call_id().as_str().to_string(),
            from_uri: answer.from().uri().to_string(),
            from_tag: answer.from().tag().expect("the INVITE carried a From-tag").to_string(),
            to_uri: answer.to().uri().to_string(),
            to_tag: answer.to().tag().expect("the 2xx carries a To-tag").to_string(),
            remote_target: answer
                .contacts()
                .as_slice()
                .first()
                .map(|c| c.uri().to_string())
                .expect("the 2xx carries a Contact"),
        }
    }
}

/// A request from `from` in the dialog `ids` names, sent as one datagram to
/// `dst`. `ruri` is the Request-URI; `to_tag` `None` sends the To untagged.
#[allow(clippy::too_many_arguments)]
async fn send_raw(
    from: &Agent,
    dst: SocketAddr,
    method: &str,
    ruri: &str,
    ids: &DialogIds,
    to_tag: Option<&str>,
    cseq: u32,
    branch: &str,
) {
    let to_tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let wire = format!(
        "{method} {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch=z9hG4bK-{branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <{from_uri}>;tag={from_tag}\r\n\
         To: <{to_uri}>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\n\
         Content-Length: 0\r\n\r\n",
        via = from.addr(),
        from_uri = ids.from_uri,
        from_tag = ids.from_tag,
        to_uri = ids.to_uri,
        call_id = ids.call_id,
    );
    from.try_send_datagram(wire.as_bytes(), dst).await.expect("the request leaves");
}

/// Every response queued at `agent` whose CSeq names `method`, after the
/// fabric has had `wait` to deliver.
async fn responses_to(
    h: &Harness,
    agent: &Agent,
    method: &str,
    wait: Duration,
) -> Vec<SipResponse> {
    h.advance(wait).await;
    let mut out = Vec::new();
    while let Some(msg) = agent.take_queued().await {
        if let SipMessage::Response(r) = msg {
            if r.cseq().method().as_str() == method {
                out.push(r);
            }
        }
    }
    out
}

/// Set up a call by hand and keep the caller's 2xx: the dialog ids a test
/// needs to write requests the scenario stack would never send.
async fn establish_keeping_answer(s: &B2buaScene) -> (scenario_harness::Dialog, SipResponse) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    (dialog, answer)
}

/// The bare address a peer might target instead of the remote target: the
/// Request-URI carries no `callRef`.
fn bare_uri(addr: SocketAddr) -> String {
    format!("sip:{addr}")
}

/// A BYE (and any other in-dialog request but ACK) for a dialog that has
/// ended, addressed without the routing key, still draws 481: the peer's
/// transaction would otherwise retransmit to Timer F and give up (RFC 3261
/// §12.2.2). The ACK draws nothing and is the one drop.
#[tokio::test(start_paused = true)]
async fn an_unkeyed_in_dialog_request_for_an_ended_dialog_draws_481() {
    let s = B2buaScene::new("b2bua-unroutable-in-dialog-481").await;
    let (mut dialog, answer) = establish_keeping_answer(&s).await;
    let ids = DialogIds::of(&answer);
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "requests in a dialog the B2BUA no longer holds are the deviation under test",
    );

    let ruri = bare_uri(s.b2bua.addr);
    let next = dialog.local_cseq() + 1;
    send_raw(&s.alice, s.b2bua.addr, "BYE", &ruri, &ids, Some(&ids.to_tag), next, "late-bye").await;
    let byes = responses_to(&s.h, &s.alice, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(
        byes.iter().map(SipResponse::status).collect::<Vec<_>>(),
        vec![481],
        "a BYE naming no dialog here is answered 481 (RFC 3261 §12.2.2, §15.1.2)"
    );
    assert_eq!(byes[0].to().tag(), Some(ids.to_tag.as_str()), "the 481 echoes the To-tag");

    send_raw(&s.alice, s.b2bua.addr, "INFO", &ruri, &ids, Some(&ids.to_tag), next + 1, "late-info")
        .await;
    let infos = responses_to(&s.h, &s.alice, "INFO", Duration::from_millis(1000)).await;
    assert_eq!(
        infos.iter().map(SipResponse::status).collect::<Vec<_>>(),
        vec![481],
        "any in-dialog request but ACK is answered 481 (RFC 3261 §12.2.2)"
    );

    let invite_cseq = answer.cseq().seq();
    send_raw(&s.alice, s.b2bua.addr, "ACK", &ruri, &ids, Some(&ids.to_tag), invite_cseq, "ack")
        .await;
    let acks = responses_to(&s.h, &s.alice, "ACK", Duration::from_millis(1000)).await;
    assert!(acks.is_empty(), "an ACK draws no response (RFC 3261 §17.1.1.3)");

    let m = s.b2bua.metrics();
    assert_eq!(m.unroutable_dropped_total(), 1, "the ACK is the one drop");
    assert_eq!(m.unroutable_dropped_of("ACK"), 1);
    assert_eq!(m.unroutable_refused_of("BYE", 481), 1);
    assert_eq!(m.unroutable_refused_of("INFO", 481), 1);
    assert_eq!(m.unroutable_internal_total(), 0);
    b2bua_harness::settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// An untagged request naming no call: a BYE draws 481 (RFC 3261 §15.1.2),
/// a method this node does not serve draws 405 with its `Allow` (§8.2.1), and
/// both carry a To-tag this node minted (§8.2.6.2).
#[tokio::test(start_paused = true)]
async fn an_untagged_request_naming_no_call_draws_its_rfc_answer_under_a_fresh_tag() {
    let s = B2buaScene::new("b2bua-unroutable-untagged").await;
    s.h.allow_violation(
        "no-bye-outside-or-early-dialog",
        "a BYE naming no dialog is the deviation under test (RFC 3261 §15.1.2)",
    );
    let ids = DialogIds {
        call_id: "no-call-here@127.0.0.1".into(),
        from_uri: format!("sip:alice@{}", s.alice.addr()),
        from_tag: "a-tag".into(),
        to_uri: format!("sip:bob@{}", s.b2bua.addr),
        to_tag: String::new(),
        remote_target: bare_uri(s.b2bua.addr),
    };

    send_raw(&s.alice, s.b2bua.addr, "BYE", &ids.remote_target, &ids, None, 1, "untagged-bye")
        .await;
    let byes = responses_to(&s.h, &s.alice, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(byes.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![481]);
    assert!(byes[0].to().tag().is_some(), "a response to an untagged request carries a To-tag");

    send_raw(&s.alice, s.b2bua.addr, "MESSAGE", &ids.remote_target, &ids, None, 2, "message").await;
    let msgs = responses_to(&s.h, &s.alice, "MESSAGE", Duration::from_millis(1000)).await;
    assert_eq!(
        msgs.iter().map(SipResponse::status).collect::<Vec<_>>(),
        vec![405],
        "a method outside the node's Allow is refused 405 (RFC 3261 §8.2.1)"
    );
    assert!(msgs[0].to().tag().is_some(), "a response to an untagged request carries a To-tag");
    let allow = b2bua_harness::stated_by_response(&msgs[0], "Allow")
        .expect("a 405 states the methods the node allows (RFC 3261 §21.4.6)");
    assert!(allow.contains("INVITE") && !allow.contains("MESSAGE"), "Allow: {allow}");
    send_raw(&s.alice, s.b2bua.addr, "FROB", &ids.remote_target, &ids, None, 4, "frob").await;
    let frobs = responses_to(&s.h, &s.alice, "FROB", Duration::from_millis(1000)).await;
    assert_eq!(
        frobs.iter().map(SipResponse::status).collect::<Vec<_>>(),
        vec![405],
        "an extension method outside Allow is refused 405 (RFC 3261 §8.2.1)"
    );

    // Carrying a routing key that names no call takes the orphan path, which
    // owes the same answer under the same tag rule.
    let keyed = format!("sip:{};callRef=w0%7cnever%7cminted", s.b2bua.addr);
    send_raw(&s.alice, s.b2bua.addr, "BYE", &keyed, &ids, None, 3, "keyed-untagged").await;
    let orphan = responses_to(&s.h, &s.alice, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(orphan.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![481]);
    assert!(orphan[0].to().tag().is_some(), "the orphan path's 481 carries a To-tag too");
    assert_eq!(
        s.b2bua.txn_metrics().fallback_to_tag_used(),
        0,
        "every refusal of an untagged request carried a tag this node minted"
    );

    let m = s.b2bua.metrics();
    assert_eq!(m.unroutable_refused_of("BYE", 481), 1, "the keyed BYE was routable");
    assert_eq!(m.unroutable_refused_of("MESSAGE", 405), 1);
    assert_eq!(m.unroutable_refused_of("other", 405), 1, "an extension method's label is bounded");
    assert_eq!(m.unroutable_dropped_total(), 0, "an answered request is no drop");

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// Late messages of a released call that carry this node's routing key are
/// not unroutable. A BYE repeated inside Timer J is the server transaction's
/// to absorb: it re-draws the 200 and never reaches the router (a UAC's Timer
/// F ends its repeats before a UAS's Timer J, RFC 3261 §17.1.2.2, §17.2.2). A
/// new request in the ended dialog resolves its call ref, finds no call and
/// draws the orphan 481. Nothing is counted unroutable.
#[tokio::test(start_paused = true)]
async fn keyed_late_messages_take_the_transaction_layer_or_the_orphan_path() {
    let s = B2buaScene::new("b2bua-unroutable-keyed-late").await;
    let mut dialog = s.establish().await;
    let (mut bye, sent) = dialog
        .send_request(InDialogMethod::Bye)
        .try_send_with_request()
        .await
        .expect("the BYE leaves");
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.is_reaped()).await;

    let calls_seen = s.b2bua.metrics().creations_total();
    s.h.advance(Duration::from_secs(5)).await;
    s.alice.try_send_datagram(sent.image(), s.b2bua.addr).await.expect("the repeat leaves");
    s.h.advance(Duration::from_millis(1000)).await;
    s.alice.drain().await;
    assert_eq!(
        s.b2bua.metrics().creations_total(),
        calls_seen,
        "the repeat never reached the router: the server transaction absorbed it"
    );

    s.h.allow_violation(
        "mid-dialog-tags",
        "a request in a dialog the B2BUA no longer holds is the deviation under test",
    );
    let mut late = dialog.send_request(InDialogMethod::Bye).send().await;
    late.expect(481).await;
    assert_eq!(s.b2bua.metrics().unroutable_dropped_total(), 0, "a keyed message is routable");

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A worker process that restarted without the call it held, on the same
/// address: each peer hangs up into it. The caller's BYE carries the dead
/// process's key and draws the orphan 481; the callee's, addressed without the
/// key, draws the unroutable 481. Both dialogs end on that answer (RFC 3261
/// §12.2.1.2) instead of retransmitting to Timer F.
#[tokio::test(start_paused = true)]
async fn a_restarted_worker_answers_the_dead_processes_dialogs_481() {
    let mut s = B2buaScene::new("b2bua-unroutable-restart").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let bob_dialog = uas.dialog();
    let b_invite = uas.request().clone();

    let addr = s.b2bua.addr;
    s.b2bua.crash();
    s.h.advance(Duration::from_millis(100)).await;
    let restarted = B2buaSut::route_all_to("127.0.0.1", s.bob.addr().port())
        .start(&s.h, "b2bua", &addr.to_string())
        .await;
    // The dead process lost its call with its memory; the scene judges the
    // process that answers from now on.
    let _crashed = std::mem::replace(&mut s.b2bua, restarted);
    s.h.allow_violation(
        "mid-dialog-tags",
        "the dialogs' requests reaching a process that never held them are under test",
    );

    let mut keyed = alice_dialog.send_request(InDialogMethod::Bye).send().await;
    keyed.expect(481).await;
    assert_eq!(s.b2bua.metrics().unroutable_dropped_total(), 0, "the key resolves");
    assert_eq!(s.b2bua.metrics().unroutable_refused_total(), 0, "the orphan path answered");

    // The callee's BYE, addressed to the bare host as some UAs do.
    let ids = DialogIds {
        call_id: b_invite.call_id().as_str().to_string(),
        from_uri: b_invite.to().uri().to_string(),
        from_tag: bob_dialog.local_tag().to_string(),
        to_uri: b_invite.from().uri().to_string(),
        to_tag: b_invite.from().tag().expect("the b-leg INVITE carries a From-tag").to_string(),
        remote_target: bare_uri(addr),
    };
    let cseq = bob_dialog.local_cseq() + 1;
    send_raw(&s.bob, addr, "BYE", &ids.remote_target, &ids, Some(&ids.to_tag), cseq, "b-bye").await;
    let unkeyed = responses_to(&s.h, &s.bob, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(
        unkeyed.iter().map(SipResponse::status).collect::<Vec<_>>(),
        vec![481],
        "without the key the restarted worker still owes the BYE its 481"
    );
    assert_eq!(s.b2bua.metrics().unroutable_refused_of("BYE", 481), 1);
    assert_eq!(s.b2bua.metrics().unroutable_dropped_total(), 0, "an answered request is no drop");

    let _ = s.finish().await;
}

/// The keepalive probe to a leg that stays silent on it, then the call ends
/// before the probe's Timer F: the transaction is released from the call and
/// its timeout later reaches the router naming no call. No peer waits on it,
/// and it is not a wire message dropped.
#[tokio::test(start_paused = true)]
async fn a_released_transactions_timeout_is_not_a_wire_drop() {
    let h = Harness::new("b2bua-unroutable-released-timeout");
    let alice = h.agent("alice", "127.0.0.1:5067").await;
    let bob = h.agent("bob", "127.0.0.1:5077").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5077).start(&h, "b2bua", "127.0.0.1:5087").await;
    let _dialog = establish(&alice, &bob, b2bua.addr).await;

    // t = 30 s: the probe. Alice stays silent on it; bob answers.
    h.advance(Duration::from_secs(30)).await;
    let _silent = alice.receive("OPTIONS").await;
    bob.receive("OPTIONS").await.respond(200, "OK").await;

    // t = 35 s: the keepalive cutoff BYEs both legs; both answer, so the call
    // ends with alice's probe still inside its Timer F (t = 62 s).
    h.advance(Duration::from_secs(5)).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;

    // Past the probe's Timer F.
    h.advance(Duration::from_secs(30)).await;
    alice.drain().await;
    settle_until(|| false).await;
    assert_eq!(
        b2bua.metrics().unroutable_dropped_total(),
        0,
        "a released transaction's timeout is no wire message dropped"
    );
    assert_eq!(
        b2bua.metrics().unroutable_internal_of("timeout", "OPTIONS"),
        1,
        "the released probe's Timer F is counted as this node's own event"
    );

    b2bua_harness::settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A CANCEL arriving after the call its INVITE opened was rejected and
/// released: the transaction layer still remembers the tag the INVITE's final
/// bound (Timer L), and the 481 answers under it (RFC 3261 §9.2). The router
/// hands the layer no tag of its own to fight it with.
#[tokio::test(start_paused = true)]
async fn a_late_cancel_draws_481_under_the_rejected_invites_tag() {
    let s = B2buaScene::new("b2bua-unroutable-late-cancel").await;
    s.h.waive(
        scenario_harness::WaiverScope::rule(
            "no-cancel-after-final",
            "a CANCEL after the INVITE's final is the deviation under test",
        )
        .on_party("alice"),
    );
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(486, "Busy Here").await;
    uas.expect_ack().await;
    let busy = call.expect(486).await;
    let tag = busy.to().tag().expect("the final carries a To-tag").to_string();
    settle_until(|| s.b2bua.is_reaped()).await;

    s.h.advance(Duration::from_secs(10)).await;
    let mut cxl = call.cancel().await;
    let refused = cxl.expect(481).await;
    assert_eq!(refused.to().tag(), Some(tag.as_str()), "the 481 carries the INVITE final's tag");
    assert_eq!(s.b2bua.txn_metrics().to_tag_coerced(), 0, "the router handed over no other tag");
    assert_eq!(s.b2bua.metrics().unroutable_refused_of("CANCEL", 481), 1);

    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// A request whose To-tag names a dialog this node does not hold draws 481
/// whatever its method (RFC 3261 §12.2.2): 481 is what ends the peer's dialog
/// (§12.2.1.2), a 405 would leave it up. Both the keyed request (orphan path)
/// and the unkeyed one (unroutable) are held to it.
#[tokio::test(start_paused = true)]
async fn a_tagged_request_of_any_method_naming_no_dialog_draws_481() {
    let s = B2buaScene::new("b2bua-unroutable-tagged-any-method").await;
    let (mut dialog, answer) = establish_keeping_answer(&s).await;
    let ids = DialogIds::of(&answer);
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "requests in a dialog the B2BUA no longer holds are the deviation under test",
    );

    let next = dialog.local_cseq() + 1;
    let keyed = ids.remote_target.clone();
    send_raw(&s.alice, s.b2bua.addr, "MESSAGE", &keyed, &ids, Some(&ids.to_tag), next, "k").await;
    let r = responses_to(&s.h, &s.alice, "MESSAGE", Duration::from_millis(1000)).await;
    assert_eq!(r.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![481], "orphan path");

    let bare = bare_uri(s.b2bua.addr);
    send_raw(&s.alice, s.b2bua.addr, "MESSAGE", &bare, &ids, Some(&ids.to_tag), next + 1, "u")
        .await;
    let r = responses_to(&s.h, &s.alice, "MESSAGE", Duration::from_millis(1000)).await;
    assert_eq!(r.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![481], "unroutable");
    assert_eq!(s.b2bua.metrics().unroutable_refused_of("MESSAGE", 481), 1);

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// The dialog lookup of an unkeyed in-dialog request fails: the node cannot
/// say whether the dialog exists, so it answers 500 as the keyed path does
/// (ADR-0023), never the 481 that would end a dialog it may hold. Once the
/// store recovers, the same request draws its 481.
#[tokio::test(start_paused = true)]
async fn a_store_fault_on_an_unkeyed_lookup_draws_500_not_481() {
    let faults = StoreFaults::default();
    let armed = faults.clone();
    let s = B2buaScene::with_b2bua("b2bua-unroutable-store-fault", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).with_store_faults(armed)
    })
    .await;
    let (mut dialog, answer) = establish_keeping_answer(&s).await;
    let ids = DialogIds::of(&answer);
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.allow_violation(
        "mid-dialog-tags",
        "a request in a dialog the B2BUA no longer holds is the deviation under test",
    );

    faults.arm(StoreFaultPoint::LiveInDialog);
    let bare = bare_uri(s.b2bua.addr);
    let next = dialog.local_cseq() + 1;
    send_raw(&s.alice, s.b2bua.addr, "BYE", &bare, &ids, Some(&ids.to_tag), next, "f").await;
    let r = responses_to(&s.h, &s.alice, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(r.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![500]);
    assert_eq!(s.b2bua.metrics().store_fault_rejected_total(), 1);
    assert_eq!(s.b2bua.metrics().unroutable_refused_total(), 0, "a fault is no refusal");

    faults.disarm_all();
    send_raw(&s.alice, s.b2bua.addr, "BYE", &bare, &ids, Some(&ids.to_tag), next + 1, "r").await;
    let r = responses_to(&s.h, &s.alice, "BYE", Duration::from_millis(1000)).await;
    assert_eq!(r.iter().map(SipResponse::status).collect::<Vec<_>>(), vec![481]);

    b2bua_harness::settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
