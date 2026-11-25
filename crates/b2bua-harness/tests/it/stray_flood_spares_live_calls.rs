//! A flood of requests that can belong to no call here (stray BYEs naming
//! dialogs the worker never had, their Request-URIs naming no call) takes none
//! of the room the requests of live calls need: a live call's BYE reaches the
//! router however many strays wait, so teardown is never refused, and the
//! strays waiting do not age the queue the new-call gate reads, so a new call
//! is served while the router has turns to give it. The router takes the
//! strays only when no work of a call waits. Ahead of the transaction layer,
//! a brake engaged on the inbound queue's depth drops the strays before they
//! take the room a live call's BYE needs there.
//!
//! The router loop is held by [`RouterPace`], the transaction layer's takes
//! from the inbound queue by [`InboundPace`]. The strays come from carol,
//! whose answers are dropped before her inbox; each case's calls end by their
//! BYE.

use std::time::Duration;

use b2bua::ingress_brake::IngressBrakeConfig;
use b2bua::TurnProbes;
use b2bua_harness::{
    send_backlog, send_strays, settle_until, B2buaScene, B2buaSut, BurstAnswers, InboundPace, Pace,
    RouterPace,
};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::Agent;

/// The single SUT's ordinal.
const NODE: &str = "w0";
/// Past a datagram's flight through the harness's fabric.
const TRANSIT: Duration = Duration::from_millis(150);
/// RFC 3261 T1: a UAC's first retransmission of a request no response
/// answered (§17.1.2.2).
const T1: Duration = Duration::from_millis(500);
/// The simulated time each router turn costs while strays wait.
const PER_TURN: Duration = Duration::from_millis(10);
/// Strays that keep a router paying [`PER_TURN`] busy for two seconds.
const STREAM: usize = 200;
/// Past the queue age from which the transaction layer refuses a new call by
/// itself (the event-queue gate's refusal delay and interval, 450 ms).
const PAST_REFUSAL_AGE: Duration = Duration::from_millis(600);

/// Event-queue work sent behind the strays.
const BACKLOG: usize = 50;
/// The bound of the harness bind's inbound queue.
const INBOUND_QUEUE: usize = 256;
/// The brake's threshold, in percent of [`INBOUND_QUEUE`]: the worker's own.
const BRAKE_PCT: u32 = 70;
/// The simulated time between two takes from the inbound queue while the
/// strays wait there.
const PER_TAKE: Duration = Duration::from_millis(1);

/// A scene whose worker's router loop is paced by the returned handle, with
/// carol, the sender of the strays, and the answers she receives.
async fn paced_scene(name: &str) -> (B2buaScene, std::sync::Arc<RouterPace>, Agent, BurstAnswers) {
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let s = B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).turn_probe(probe)
    })
    .await;
    let answers = BurstAnswers::default();
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", answers.hook()).await;
    (s, pace, carol, answers)
}

/// Strays past the event queue's bound wait behind a frozen router; the BYE
/// of a live call sent after them reaches bob on its first copy, before a
/// UAC would retransmit it, once the router runs.
#[tokio::test(start_paused = true)]
async fn a_bye_sent_behind_a_stray_flood_reaches_the_callee_on_its_first_copy() {
    let (s, pace, carol, _answers) = paced_scene("stray-flood-bye").await;
    let mut dialog = s.establish().await;

    pace.set(NODE, Pace::Frozen);
    send_strays(&carol, s.b2bua.addr, b2bua::EVENT_QUEUE_CAPACITY + 64, "flood").await;
    s.h.advance(TRANSIT).await;
    let mut bye = dialog.bye().await;
    s.h.advance(TRANSIT).await;
    pace.set(NODE, Pace::Free);

    let first_copy = tokio::time::timeout(T1, s.bob.try_receive("BYE")).await;
    let mut at_bob = first_copy
        .expect("the BYE's first copy reaches bob before a retransmission would")
        .expect("a BYE");
    at_bob.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// Strays keep a slow router busy for seconds; a new INVITE sent once the
/// first of them has waited past the refusal age is admitted, and its call
/// is set up and ended while strays still wait.
#[tokio::test(start_paused = true)]
async fn a_new_call_behind_waiting_strays_is_served() {
    let (s, pace, carol, _answers) = paced_scene("stray-flood-new-call").await;

    pace.set(NODE, Pace::PerTurn(PER_TURN));
    send_strays(&carol, s.b2bua.addr, STREAM, "stream").await;
    s.h.advance(PAST_REFUSAL_AGE).await;
    let answered = || s.b2bua.metrics().unroutable_refused_of("BYE", 481);
    assert!(answered() < STREAM as u64 / 2, "strays still wait: {} answered", answered());

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(TRANSIT * 2).await;
    assert_eq!(s.b2bua.new_calls().rejected_sum(), 0, "the new call is not refused");
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    assert!(answered() < STREAM as u64, "the call was set up while strays waited");

    s.hangup(&mut dialog).await;
    pace.set(NODE, Pace::Free);
    settle_until(|| answered() == STREAM as u64).await;
    assert_eq!(answered(), STREAM as u64, "every stray is answered once the router is free");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}

/// Strays reach a slow router ahead of event-queue work: the router answers
/// all of that work before more than the stray its turn in progress took.
#[tokio::test(start_paused = true)]
async fn waiting_strays_are_taken_after_the_work_of_the_event_queue() {
    let (s, pace, carol, answers) = paced_scene("stray-flood-taken-last").await;

    pace.set(NODE, Pace::PerTurn(PER_TURN));
    send_strays(&carol, s.b2bua.addr, STREAM, "last").await;
    send_backlog(&carol, s.b2bua.addr, BACKLOG, "first").await;
    settle_until(|| answers.of(405) == BACKLOG as u64).await;
    assert_eq!(answers.of(405), BACKLOG as u64, "the backlog is answered 405");
    assert!(answers.of(481) <= 2, "at most the turns in progress: {} strays", answers.of(481));

    pace.set(NODE, Pace::Free);
    settle_until(|| answers.of(481) == STREAM as u64).await;
    assert_eq!(answers.of(481), STREAM as u64, "then every stray is answered");
    let _report = s.finish().await;
}

/// Strays past the inbound queue's bound reach a worker whose transaction
/// layer takes a datagram a millisecond; the BYE of a live call sent right
/// behind them finds room in that queue and reaches bob.
#[tokio::test(start_paused = true)]
async fn a_bye_behind_a_stray_flood_past_the_inbound_queue_reaches_the_callee() {
    let pace = InboundPace::new();
    let inbound = pace.clone();
    let s = B2buaScene::with_b2bua("stray-flood-inbound", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port)
            .inbound_pace(inbound)
            .ingress_brake(IngressBrakeConfig::by_depth(INBOUND_QUEUE, BRAKE_PCT))
    })
    .await;
    let answers = BurstAnswers::default();
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", answers.hook()).await;
    let mut dialog = s.establish().await;

    pace.set(PER_TAKE);
    send_strays(&carol, s.b2bua.addr, INBOUND_QUEUE * 2, "inbound").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    pace.set(Duration::ZERO);
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let _report = s.finish().await;
}
