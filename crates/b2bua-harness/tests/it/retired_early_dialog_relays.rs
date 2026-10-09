//! The relays still open in early dialogs the answer retires (RFC 3261
//! §12.1.2, §8.1.3.3). A request one side relayed into an early dialog the
//! answer drops — a losing callee fork's toward the caller, the caller's
//! toward a losing fork, or one shown under a caller-facing tag a fresh-tag
//! answer supersedes — still gets exactly one final, at its own originator:
//! never another dialog's transaction carrying the same CSeq, never the
//! answered session's description state, and never nothing at call end.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_message::header::RSeq;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";

fn cseq_of(txn: &ServerTxn) -> u32 {
    txn.request().cseq().seq()
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

/// The caller's request relayed into a callee fork the answer then drops
/// still gets its final: fork 2 rings, alice UPDATEs fork 2's dialog, fork 1
/// answers before fork 2 replies, and fork 2's own 200 reaches her UPDATE in
/// fork 2's dialog — never left unanswered, never delivered to fork 1's.
#[tokio::test(start_paused = true)]
async fn a_caller_request_into_a_dropped_fork_still_gets_its_final() {
    let h = Harness::with_transit_delay("b2bua-caller-request-into-dropped-fork", 1);
    let alice = h.agent("alice", "127.0.0.1:7339").await;
    let bob = h.agent("bob", "127.0.0.1:7340").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7340).start(&h, "b2bua", "127.0.0.1:7341").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();

    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&a_tag2).send().await;
    let mut at_fork2 = bob.receive("UPDATE").await;
    assert_eq!(at_fork2.request().to().tag(), Some("bobfork2"), "relayed into fork 2");

    uas.adopt_to_tag("bobfork1");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag1.as_str()), "fork 1 answers");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    at_fork2.respond(200, "OK").await;
    let answered = update.expect(200).await;
    assert_eq!(
        answered.to().tag(),
        Some(a_tag2.as_str()),
        "her UPDATE is answered in fork 2's dialog"
    );

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

/// Answers the caller under a fresh To-tag on the callee's 2xx, superseding
/// the early dialog she was shown (`AnswerALegNewDialog`), and bridges.
mod fresh_tag {
    use b2bua::rules::{
        Effect, Match, RelayedFinal, RuleAction, RuleCall, RuleContext, RuleDefinition,
        RuleHandleResult, ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, Direction, LegDisposition, LegState, TimerType};
    use sip_message::generators::SourceBody;

    define_service! {
        id: "fresh-tag",
        machine: FRESH,
        states: FreshState { Waiting },
        init: |_call: &RuleCall| Some(ServiceSeed::new(FreshState::Waiting.label())),
        rules: [ answer_under_a_fresh_tag() ],
    }

    fn answer_under_a_fresh_tag() -> RuleDefinition {
        sm_rule! {
            id: "fresh-tag-answer",
            machine: FRESH,
            active: [ FreshState::Waiting ],
            transitions: [ FreshState::Waiting => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 under a fresh To-tag" },
                Effect::GuardTimer { timer: TimerType::SetupTimeout, label: "disarm the setup timers" },
                Effect::LifecycleCommand { label: "bridge caller and callee" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early]),
            handle: |ctx: &RuleContext| {
                let callee_final = ctx.response()?;
                let b = ctx.source_leg_id.to_string();
                let a = ctx.call.a_leg().leg_id.clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::UpdateLegState {
                        leg_id: b.clone(),
                        state: LegState::Confirmed,
                        disposition: Some(LegDisposition::Bridged),
                    },
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::AnswerALegNewDialog {
                        status: 200,
                        reason: "OK".into(),
                        body: Some(b2bua_sdk::model::Body::from_leg(
                            callee_final.body().to_vec(),
                            b.clone(),
                        )),
                        to_tag: None,
                        header_updates: vec![],
                        relayed: RelayedFinal::of(
                            callee_final,
                            ctx.call.a_leg_invite(),
                            200,
                            SourceBody::Verbatim,
                            ctx.config,
                        ),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: b.clone(),
                        status_code: Some(200),
                        reason: None,
                    },
                    RuleAction::CancelTimer { id: format!("NoAnswer:{b}") },
                    RuleAction::CancelTimer { id: format!("{:?}", TimerType::SetupTimeout) },
                    RuleAction::Merge { leg_a: a, leg_b: b },
                    RuleAction::ClearState { machine: FRESH },
                ]))
            },
        }
    }
}

/// The callee's INFO reaches the caller in the early dialog A1 her 183 showed;
/// the answer comes under a fresh tag A2 before she replies. Her final to the
/// INFO, under A1, answers the callee's INFO — `status` 200 or 481 alike — and
/// the call goes on: the callee's next INFO, in A2, is answered on its own.
async fn fresh_tag_answer_leaves_the_early_info_its_final(
    name: &str,
    ports: [u16; 3],
    status: u16,
) {
    let h = Harness::with_transit_delay(name, 1);
    let alice = h.agent("alice", &format!("127.0.0.1:{}", ports[0])).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{}", ports[1])).await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", ports[1])
        .services(vec![fresh_tag::service_def()])
        .start(&h, "b2bua", &format!("127.0.0.1:{}", ports[2]))
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").with_sdp(ANSWER).await;
    let a1 = call.expect(183).await.to().tag().expect("the early dialog's tag").to_string();

    let mut bob_dialog = uas.dialog();
    let mut info1 = bob_dialog.request(InDialogMethod::Info, None).await;
    let mut at_alice1 = alice.receive("INFO").await;
    assert_eq!(at_alice1.request().from().tag(), Some(a1.as_str()), "relayed in A1");

    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    let a2 = ok.to().tag().expect("the answer's tag").to_string();
    assert_ne!(a2, a1, "the answer comes under a fresh tag");
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    at_alice1
        .respond(status, if status == 200 { "OK" } else { "Call/Transaction Does Not Exist" })
        .await;
    let answered = info1.expect(status).await;
    assert_eq!(answered.cseq().seq(), 1, "the callee's first INFO is the one answered");

    let mut info2 = bob_dialog.request(InDialogMethod::Info, None).await;
    let mut at_alice2 = alice.receive("INFO").await;
    assert_eq!(at_alice2.request().from().tag(), Some(a2.as_str()), "relayed in A2");
    at_alice2.respond(200, "OK").await;
    let answered = info2.expect(200).await;
    assert_eq!(answered.cseq().seq(), 2, "the callee's second INFO is the one answered");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_fresh_tag_answer_leaves_the_early_info_its_200() {
    fresh_tag_answer_leaves_the_early_info_its_final(
        "b2bua-fresh-tag-early-info-200",
        [7342, 7343, 7344],
        200,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_fresh_tag_answer_leaves_the_early_info_its_481() {
    fresh_tag_answer_leaves_the_early_info_its_final(
        "b2bua-fresh-tag-early-info-481",
        [7345, 7346, 7347],
        481,
    )
    .await;
}

const F1_ANSWER: &str = "v=0\r\no=bob 11 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const F1_REOFFER: &str = "v=0\r\no=bob 11 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const F2_ANSWER: &str = "v=0\r\no=bob 22 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n";
const F2_REANSWER: &str = "v=0\r\no=bob 22 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20022 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";
const ALICE_HOLD: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";
const ALICE_HELD: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=recvonly\r\n";

/// The `o=` session version of `body`.
fn o_version(body: &[u8]) -> u64 {
    let text = String::from_utf8_lossy(body);
    let o = text.lines().find(|l| l.starts_with("o=")).expect("an o= line");
    o.split_whitespace().nth(2).and_then(|v| v.parse().ok()).expect("a session version")
}

/// RFC 3264 §8 in the answered dialog: a losing fork's description, relayed
/// to the caller as its answer to her UPDATE in that fork's retired dialog,
/// is no description of the answered session — the next one she gets there
/// is one version above the answer's.
#[tokio::test(start_paused = true)]
async fn a_retired_relays_description_leaves_the_answered_session_alone() {
    let h = Harness::with_transit_delay("b2bua-retired-relay-description", 1);
    let alice = h.agent("alice", "127.0.0.1:7348").await;
    let bob = h.agent("bob", "127.0.0.1:7349").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7349).start(&h, "b2bua", "127.0.0.1:7350").await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    let a_tag1 = call.expect(180).await.to().tag().expect("fork 1's a-facing tag").to_string();
    uas.respond(183, "Session Progress")
        .with_to_tag("bobfork2")
        .reliable(1)
        .with_sdp(F2_ANSWER)
        .await;
    let p183 = call.expect(183).await;
    let a_tag2 = p183.to().tag().expect("fork 2's a-facing tag").to_string();
    let rseq = p183.header::<RSeq>().expect("an RSeq").expect("readable RSeq").value();
    let mut prack = call
        .send_request(InDialogMethod::Prack)
        .with_to_tag(&a_tag2)
        .with_rack(&format!("{rseq} 1 INVITE"))
        .send()
        .await;
    bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;

    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&a_tag2)
        .with_sdp(ALICE_HOLD)
        .send()
        .await;
    let mut at_fork2 = bob.receive("UPDATE").await;

    uas.adopt_to_tag("bobfork1");
    uas.respond(200, "OK").with_sdp(F1_ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.to().tag(), Some(a_tag1.as_str()), "fork 1 answers");
    let answered = o_version(ok.body());
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    at_fork2.respond(200, "OK").with_sdp(F2_REANSWER).await;
    update.expect(200).await;

    let mut fork1 = uas.dialog();
    let mut reinvite = fork1.request(InDialogMethod::Invite, Some(F1_REOFFER)).await;
    let mut at_alice = alice.receive("INVITE").await;
    assert_eq!(
        o_version(at_alice.request().body()),
        answered + 1,
        "the answered session's next description, one above its answer"
    );
    at_alice.respond(200, "OK").with_sdp(ALICE_HELD).await;
    reinvite.expect(200).await;
    fork1.ack_for(1, None).await;
    alice.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}

/// A relay still open in a retired early dialog when the call ends is
/// answered there like any other (RFC 3261 §8.2.6): the caller's UPDATE into
/// fork 2, which fork 2 never answers before the BYE ends the call, draws 481;
/// fork 2's own final, coming late, is absorbed.
#[tokio::test(start_paused = true)]
async fn a_retired_relay_open_at_call_end_is_answered() {
    let h = Harness::with_transit_delay("b2bua-retired-relay-at-call-end", 1);
    let alice = h.agent("alice", "127.0.0.1:7351").await;
    let bob = h.agent("bob", "127.0.0.1:7352").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 7352).start(&h, "b2bua", "127.0.0.1:7353").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("bobfork1").await;
    call.expect(180).await;
    uas.respond(180, "Ringing").with_to_tag("bobfork2").await;
    let a_tag2 = call.expect(180).await.to().tag().expect("fork 2's a-facing tag").to_string();

    let mut update = call.send_request(InDialogMethod::Update).with_to_tag(&a_tag2).send().await;
    let mut at_fork2 = bob.receive("UPDATE").await;

    uas.adopt_to_tag("bobfork1");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    update.expect(481).await;

    at_fork2.respond(200, "OK").await;

    settle_until(|| b2bua.is_reaped()).await;
    alice.drain().await;
    bob.drain().await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
