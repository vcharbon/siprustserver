//! A party holding the peer's re-INVITE (RFC 3261 §14.1, §14.2, §17.2.1):
//! the held INVITE gets its 100 Trying, a second offer meanwhile is refused
//! 500 with Retry-After, and the party's own re-INVITE waits until the peer's
//! is answered and ACKed. A request requiring an unknown extension is refused
//! 420, a re-INVITE's refusal awaiting its hop-ACK.

use std::time::Duration;

use sip_message::generators::InDialogMethod;

use super::testkit::caller_spec;
use crate::actor::answers::{Answer, Answers};
use crate::actor::*;
use crate::{Harness, ANSWER_SDP};

/// Bob's own description moved on: his re-offer, then his answer to alice's.
const BOB_V2: &str =
    "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const BOB_V3: &str =
    "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20003 RTP/AVP 0\r\n";

/// Alice calls bob, holding his renegotiations as `answers` states, and runs
/// `goals` after the INVITE.
fn alice_calls(
    alice: &crate::Agent,
    bob: &crate::Agent,
    answers: Answers,
    mut goals: Vec<Goal>,
) -> CallPlan {
    goals.insert(0, Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }));
    let mut spec = caller_spec("alice", alice, ("bob", bob.clone()), goals);
    spec.answers = answers;
    CallPlan {
        actors: vec![spec],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// The indexes in the harness wire of the datagrams from `from` whose start
/// line begins with `start`.
fn wire_indexes(h: &Harness, from: &str, start: &str) -> Vec<usize> {
    let entries = h.wire_entries();
    let sent = entries.iter().filter(|e| e.reemit.is_none()).enumerate();
    sent.filter(|(_, e)| e.from.to_string() == from && e.raw.starts_with(start.as_bytes()))
        .map(|(i, _)| i)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_held_reinvite_is_trying_refuses_a_second_offer_and_gates_her_own_until_its_ack() {
    let h = Harness::new("actor-held-reinvite-gate").describe(
        "alice holds bob's re-INVITE 2 s: it gets 100 Trying, his UPDATE meanwhile 500 with \
         Retry-After; her own re-INVITE leaves only after bob's late ACK",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            let mut dialog = invite.dialog();
            bob.receive("ACK").await;
            let mut reinvite = dialog.reinvite(Some(BOB_V2)).await;
            reinvite.expect(100).await;
            let mut update = dialog.send_request(InDialogMethod::Update).send().await;
            let refused = update.expect(500).await;
            assert!(refused.raw(sip_message::HeaderName::RetryAfter).next().is_some());
            let ok = reinvite.expect(200).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            dialog.ack_for(ok.cseq().seq(), None).await;
            let mut hers = bob.receive("INVITE").await;
            hers.respond(200, "OK").with_sdp(BOB_V3).await;
            bob.receive("ACK").await;
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let hold = Answer { hold: Duration::from_secs(2), refuse: None };
    let answers = Answers { reinvite: hold, ..Default::default() };
    let goals = vec![
        Goal::new(Barrier::received("got_his", "alice", "INVITE"), GoalStep::Reinvite),
        Goal::new(Barrier::pred("hers_done", |s| s.leg("alice").reneg_count() >= 1), GoalStep::Bye),
    ];
    let verdict =
        run_call(alice_calls(&alice, &bob, answers, goals), Duration::from_secs(20)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    let his_ack = wire_indexes(&h, "127.0.0.1:5070", "ACK ")[0];
    let hers = *wire_indexes(&h, "127.0.0.1:5060", "INVITE ").last().expect("alice re-INVITEd");
    assert!(his_ack < hers, "her re-INVITE waited for his ACK");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_requiring_an_unknown_extension_is_refused_420_and_hop_acked() {
    let h = Harness::new("actor-reinvite-require-420").describe(
        "bob's re-INVITE requires an extension alice's actor does not implement: she refuses it \
         420 naming it, his ACK closes the refusal, and the call goes on to her BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            let mut dialog = invite.dialog();
            bob.receive("ACK").await;
            let mut reinvite = dialog
                .send_request(InDialogMethod::Invite)
                .with_header("Require", "x-nonexistent")
                .with_body("application/sdp", BOB_V2.as_bytes().to_vec())
                .send()
                .await;
            let refused = reinvite.expect(420).await;
            let unsupported: Vec<&str> =
                refused.raw(sip_message::HeaderName::Unsupported).collect();
            assert_eq!(unsupported, ["x-nonexistent"]);
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let refused = |s: &StateInner| s.leg("alice").received_method("INVITE");
    let goals = vec![Goal::new(Barrier::pred("refused", refused), GoalStep::Bye)];
    let call = alice_calls(&alice, &bob, Answers::default(), goals);
    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the refusal's hop-ACK closes it, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_bye_requiring_an_unknown_extension_is_refused_420_and_the_dialog_stays_up() {
    let h = Harness::new("actor-bye-require-420").describe(
        "bob's BYE requires an extension alice's actor does not implement: she refuses it 420 and \
         the dialog stays up until his plain BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            let mut dialog = invite.dialog();
            bob.receive("ACK").await;
            let mut demanding = dialog
                .send_request(InDialogMethod::Bye)
                .with_header("Require", "x-nonexistent")
                .send()
                .await;
            demanding.expect(420).await;
            dialog.bye().await.expect(200).await;
        })
    };
    let call = alice_calls(&alice, &bob, Answers::default(), vec![]);
    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the plain BYE ends the call, got {verdict:?}");
    bob_task.await.expect("bob's demanding BYE drew 420, his plain one 200");
    h.finish().await;
}
