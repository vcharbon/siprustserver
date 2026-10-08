//! Each early dialog the caller is shown is a dialog of its own (RFC 3261
//! §12.1, §12.1.2): its own tag, its own CSeq sequence in each direction
//! (§12.2.1.1), measured on its own (§12.2.2).
//!
//! A forked callee shows the caller one early dialog per fork, each under the
//! a-facing tag of that fork's provisionals. A request a fork sends is relayed
//! to the caller in THAT dialog, numbered from that dialog's own sequence; the
//! answer confirms one of them, which keeps its sequence. The caller numbers
//! each early dialog separately, and a request of hers below the last one its
//! dialog took is out of order.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Agent, Harness, ServerTxn, WaiverScope};
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";

fn cseq_of(txn: &ServerTxn) -> u32 {
    txn.request().cseq().seq()
}

fn assert_contiguous(seen: &[u32], what: &str) {
    let expected: Vec<u32> = (0..seen.len() as u32).map(|i| seen[0] + i).collect();
    assert_eq!(seen, expected.as_slice(), "{what}: one more than the last, every time");
}

/// A bodiless UPDATE on the callee's `dialog`, relayed to the caller, who
/// answers it 200: the From-tag and CSeq the caller saw.
async fn update_toward_caller(
    dialog: &mut scenario_harness::Dialog,
    alice: &Agent,
) -> (String, u32) {
    let mut update = dialog.request(InDialogMethod::Update, None).await;
    let mut at_alice = alice.receive("UPDATE").await;
    let seen =
        (at_alice.request().from().tag().unwrap_or_default().to_string(), cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    update.expect(200).await;
    seen
}

/// A forked callee's early requests reach the caller each in its own fork's
/// early dialog, numbered from that dialog's sequence however the forks
/// interleave; fork 2 answers, and its re-INVITE and BYE continue fork 2's
/// run in fork 2's dialog.
///
/// ```text
///   fork 1: UPDATE             → caller, a-tag 1: n
///   fork 2: UPDATE             → caller, a-tag 2: m
///   fork 1: UPDATE             → caller, a-tag 1: n+1
///   fork 2: UPDATE             → caller, a-tag 2: m+1
///   fork 1: UPDATE             → caller, a-tag 1: n+2
///   200 (INVITE, fork 2)       → caller dialog a-tag 2
///   fork 2: re-INVITE, BYE     → caller, a-tag 2: m+2, m+3
/// ```
#[tokio::test(start_paused = true)]
async fn a_forked_callee_request_rides_its_own_caller_facing_dialog() {
    let h = Harness::with_transit_delay("b2bua-caller-facing-early-dialog-per-fork", 1);
    let alice = h.agent("alice", "127.0.0.1:7321").await;
    let bob = h.agent("bob", "127.0.0.1:7322").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7322).start(&h, "b2bua", "127.0.0.1:7323").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();
    assert_ne!(a_tag1, a_tag2, "each callee fork maps to its own a-facing tag");

    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();
    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();

    let mut on_tag1 = Vec::new();
    let mut on_tag2 = Vec::new();
    for _ in 0..2 {
        let (tag, cseq) = update_toward_caller(&mut fork1, &alice).await;
        assert_eq!(tag, a_tag1, "fork 1's UPDATE rides fork 1's caller-facing dialog");
        on_tag1.push(cseq);
        let (tag, cseq) = update_toward_caller(&mut fork2, &alice).await;
        assert_eq!(tag, a_tag2, "fork 2's UPDATE rides fork 2's caller-facing dialog");
        on_tag2.push(cseq);
    }
    let (tag, cseq) = update_toward_caller(&mut fork1, &alice).await;
    assert_eq!(tag, a_tag1, "fork 1's UPDATE rides fork 1's caller-facing dialog");
    on_tag1.push(cseq);
    assert_contiguous(&on_tag1, "fork 1's early dialog toward alice");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag2.as_str()), "fork 2 answers");
    let _alice_dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut reinvite = fork2.request(InDialogMethod::Invite, Some(BOB_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    assert_eq!(at_alice.request().from().tag(), Some(a_tag2.as_str()));
    on_tag2.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinvite.expect(200).await;
    fork2.ack_for(3, None).await;
    alice.receive("ACK").await;

    let mut bye = fork2.bye().await;
    let mut at_alice = alice.receive("BYE").await;
    assert_eq!(at_alice.request().from().tag(), Some(a_tag2.as_str()));
    on_tag2.push(cseq_of(&at_alice));
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&on_tag2, "fork 2's dialog toward alice, early and confirmed");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A strict caller numbers each early dialog from her INVITE: three UPDATEs on
/// fork 1's dialog (2, 3, 4), then one on fork 2's (2) — in order on fork 2's
/// own dialog, so relayed. Fork 2 answers; her reuse of 1 there is below the
/// 2 that dialog took, so it is out of order (§12.2.2): 500, not relayed. Her
/// BYE (3) continues fork 2's dialog and is in order.
#[tokio::test(start_paused = true)]
async fn the_caller_is_measured_per_caller_facing_dialog() {
    let h = Harness::with_transit_delay("b2bua-caller-measured-per-early-dialog", 1);
    let alice = h.agent("alice", "127.0.0.1:7324").await;
    let bob = h.agent("bob", "127.0.0.1:7325").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7325).start(&h, "b2bua", "127.0.0.1:7326").await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "alice reuses a spent CSeq after the answer: the out-of-order request under test (RFC 3261 §12.2.2)",
        )
        .on_party("alice"),
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();

    for (a_tag, b_tag, cseq) in [
        (&a_tag1, "bobfork1", 2),
        (&a_tag1, "bobfork1", 3),
        (&a_tag1, "bobfork1", 4),
        (&a_tag2, "bobfork2", 2),
    ] {
        let mut update = call.send_request(InDialogMethod::Update).with_to_tag(a_tag).send().await;
        let mut at_bob = bob.receive("UPDATE").await;
        assert_eq!(at_bob.request().to().tag(), Some(b_tag), "relayed into the fork it named");
        at_bob.respond(200, "OK").await;
        let ok = update.expect(200).await;
        assert_eq!(ok.cseq().seq(), cseq, "alice numbers each early dialog on its own");
    }

    uas.adopt_to_tag("bobfork2");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag2.as_str()), "fork 2 answers");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(dialog.local_cseq(), 2, "fork 2's dialog spent 1 and 2");

    dialog.set_local_cseq(0);
    let mut stale = dialog.request(InDialogMethod::Info, None).await;
    dialog.set_local_cseq(2);
    stale.expect(500).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("INFO", &[]).await.is_none(),
        "an out-of-order request is not relayed"
    );

    let mut bye = dialog.bye().await;
    let mut at_bob = bob.receive("BYE").await;
    assert_eq!(at_bob.request().to().tag(), Some("bobfork2"));
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// §12.2.2 on the caller's side of an unforked call: her early UPDATEs spent
/// 2 and 3, so a request of hers after the answer under CSeq 2 is out of
/// order: 500, not relayed. Her next request continues her own sequence toward
/// the callee.
#[tokio::test(start_paused = true)]
async fn an_out_of_order_caller_request_is_answered_500() {
    let h = Harness::with_transit_delay("b2bua-caller-stale-cseq-after-answer", 1);
    let alice = h.agent("alice", "127.0.0.1:7327").await;
    let bob = h.agent("bob", "127.0.0.1:7328").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7328).start(&h, "b2bua", "127.0.0.1:7329").await;
    h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "alice reuses a spent CSeq after the answer: the out-of-order request under test (RFC 3261 §12.2.2)",
        )
        .on_party("alice"),
    );

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let a_tag = call.expect(180).await.to().tag().expect("the a-facing tag").to_string();

    let mut seen = Vec::new();
    for _ in 0..2 {
        let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&a_tag).send().await;
        let mut at_bob = bob.receive("UPDATE").await;
        seen.push(cseq_of(&at_bob));
        at_bob.respond(200, "OK").await;
        update.expect(200).await;
    }

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let spent = dialog.local_cseq();

    dialog.set_local_cseq(spent - 2);
    let mut stale = dialog.request(InDialogMethod::Info, None).await;
    dialog.set_local_cseq(spent);
    stale.expect(500).await;
    h.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("INFO", &[]).await.is_none(),
        "an out-of-order request is not relayed"
    );

    let mut bye = dialog.bye().await;
    let mut at_bob = bob.receive("BYE").await;
    seen.push(cseq_of(&at_bob));
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    assert_contiguous(&seen, "alice's two UPDATEs and BYE toward bob");

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The caller's answer to a request relayed in an early dialog the answer
/// retired reaches that request's originator, never a request of the answered
/// dialog that happens to carry the same CSeq: fork 2's UPDATE and fork 1's
/// both reach her as CSeq 2, each in its own dialog; fork 1 answers; her late
/// 200 to fork 2's UPDATE answers fork 2, and her 200 to fork 1's answers fork 1.
#[tokio::test(start_paused = true)]
async fn a_late_answer_in_a_retired_dialog_reaches_its_own_originator() {
    let h = Harness::with_transit_delay("b2bua-late-answer-retired-early-dialog", 1);
    let alice = h.agent("alice", "127.0.0.1:7333").await;
    let bob = h.agent("bob", "127.0.0.1:7334").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7334).start(&h, "b2bua", "127.0.0.1:7335").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();

    uas.adopt_to_tag("bobfork2");
    let mut fork2 = uas.dialog();
    uas.adopt_to_tag("bobfork1");
    let mut fork1 = uas.dialog();

    let mut update2 = fork2.request(InDialogMethod::Update, None).await;
    let mut at_alice2 = alice.receive("UPDATE").await;
    assert_eq!(at_alice2.request().from().tag(), Some(a_tag2.as_str()));
    let mut update1 = fork1.request(InDialogMethod::Update, None).await;
    let mut at_alice1 = alice.receive("UPDATE").await;
    assert_eq!(at_alice1.request().from().tag(), Some(a_tag1.as_str()));
    assert_eq!(cseq_of(&at_alice1), cseq_of(&at_alice2), "two dialogs, one number each");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag1.as_str()), "fork 1 answers");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    at_alice2.respond(200, "OK").await;
    let answered2 = update2.expect(200).await;
    assert_eq!(answered2.from().tag(), Some("bobfork2"), "fork 2's UPDATE is the one answered");
    at_alice1.respond(200, "OK").await;
    let answered1 = update1.expect(200).await;
    assert_eq!(answered1.from().tag(), Some("bobfork1"));

    let mut bye = dialog.bye().await;
    let mut at_bob = bob.receive("BYE").await;
    assert_eq!(at_bob.request().to().tag(), Some("bobfork1"));
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// The service that shows the caller a second early dialog of its own: the
/// callee's 183 is answered toward her as a 183 under a To-tag the service
/// states, and is not relayed.
mod restamp {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, LegState};

    pub const TAG: &str = "service-face";

    define_service! {
        id: "restamp",
        machine: RESTAMP,
        states: RestampState { Waiting },
        init: |_call: &RuleCall| Some(ServiceSeed::new(RestampState::Waiting.label())),
        rules: [ restamp_183() ],
    }

    fn restamp_183() -> RuleDefinition {
        sm_rule! {
            id: "restamp-183",
            machine: RESTAMP,
            active: [ RestampState::Waiting ],
            transitions: [ RestampState::Waiting => Terminal ],
            effects: [
                Effect::Provisional { status: 183, label: "183 under the service's own To-tag" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_code(183)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early]),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::SendProvisionalToLeg {
                        leg_id: ctx.call.a_leg().leg_id.clone(),
                        status: 183,
                        reason: "Session Progress".into(),
                        body: None,
                        to_tag: Some(TAG.into()),
                        p_early_media: None,
                    },
                    RuleAction::ClearState { machine: RESTAMP },
                ]))
            },
        }
    }
}

/// A request the caller sends in an early dialog whose tag names no
/// caller-facing record moves no other dialog's sequence: she spends 2 and 3
/// on the callee's dialog (the 180's tag), then her own 2 on the dialog a
/// service's 183 showed her under its own tag — in order there, so relayed.
#[tokio::test(start_paused = true)]
async fn a_request_on_an_unrecorded_caller_facing_tag_moves_no_other_sequence() {
    let h = Harness::with_transit_delay("b2bua-unrecorded-caller-facing-tag", 1);
    let alice = h.agent("alice", "127.0.0.1:7336").await;
    let bob = h.agent("bob", "127.0.0.1:7337").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 7337)
        .services(vec![restamp::service_def()])
        .start(&h, "b2bua", "127.0.0.1:7338")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing_tag = call.expect(180).await.to().tag().expect("the 180's tag").to_string();
    uas.respond(183, "Session Progress").await;
    let shown = call.expect(183).await;
    assert_eq!(shown.to().tag(), Some(restamp::TAG), "the service's own early dialog");

    for (tag, cseq) in [(ringing_tag.as_str(), 2), (ringing_tag.as_str(), 3), (restamp::TAG, 2)] {
        let mut update = call.send_request(InDialogMethod::Update).with_to_tag(tag).send().await;
        let mut at_bob = bob.receive("UPDATE").await;
        at_bob.respond(200, "OK").await;
        let ok = update.expect(200).await;
        assert_eq!(ok.cseq().seq(), cseq, "alice numbers each early dialog on its own");
    }

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
