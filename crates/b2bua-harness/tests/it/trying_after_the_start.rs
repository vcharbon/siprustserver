//! An initial INVITE's 100 Trying tells the previous hop that its call is
//! being worked on (RFC 3261 §17.2.1), so it leaves only once the call starts:
//! while the router has not taken the INVITE's turn, the node sends nothing and
//! absorbs the caller's retransmissions unanswered, and the previous hop's own
//! timers fail the call over. A turn that starts inside the start bound, counted
//! from the INVITE's receipt, sends the 100 and the call goes on. A turn past it
//! dials no one and answers the bare 503 of a refused new call. An emergency
//! INVITE is held to the same bound. A re-INVITE keeps its §17.2.1 bound from
//! its receipt (ADR-0070).
//!
//! Every call here ends fully: a refused call with its CDR, an answered one
//! hung up by its caller.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::config::B2buaConfig;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::new_calls::Refusal;
use b2bua::TurnProbes;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, Pace, RouterPace};
use call::TerminationCause;
use scenario_harness::callflow::{hangup, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, ClientInvite};
use sip_message::header::HeaderName;

/// The scene's b2bua node, whose router loop the pace holds.
const NODE: &str = "w0";
/// One hop on the harness network.
const TRANSIT: Duration = Duration::from_millis(100);
/// RFC 3261 T1: the caller's first INVITE retransmission is due this long
/// after the INVITE, the second 2·T1 after the first (Timer A, §17.1.1.2).
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 §17.2.1: the bound on an INVITE's first provisional.
const TRYING_BOUND: Duration = Duration::from_millis(200);
/// How far either side of its due instant a response is read.
const MARGIN: Duration = Duration::from_millis(20);
/// The start bound at its default.
const START_BOUND: Duration =
    Duration::from_millis(B2buaConfig::DEFAULT_CALL_START_BOUND_MS.unsigned_abs());
/// How long the router stays wedged, from the INVITE's send, for a turn that
/// starts well inside the start bound: past the layer's 200 ms and the first
/// retransmission.
const WEDGED_INSIDE: Duration = Duration::from_millis(1_200);
/// How long the router stays wedged, from the INVITE's send, for a turn that
/// starts well past the start bound, and well inside the 5 s setup budget:
/// past two retransmissions.
const WEDGED_PAST: Duration = Duration::from_millis(2_500);
/// A wedge less the INVITE's transit to the node is how long its turn waits
/// after the receipt: inside the start bound for one, past it for the other.
const _: () = assert!(WEDGED_INSIDE.as_millis() < START_BOUND.as_millis() + TRANSIT.as_millis());
const _: () = assert!(WEDGED_PAST.as_millis() > START_BOUND.as_millis() + TRANSIT.as_millis());

/// A scene whose b2bua routes every call to bob, its router loop under `pace`.
async fn scene(name: &str, pace: &std::sync::Arc<RouterPace>) -> B2buaScene {
    let probe = TurnProbes::new(pace.clone());
    B2buaScene::with_b2bua(name, |bob_port| {
        let engine = ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port);
        B2buaSut::builder(std::sync::Arc::new(engine)).turn_probe(probe)
    })
    .await
}

/// Alice's pending INVITE to bob through the scene's b2bua, of `class`: an
/// emergency one carries its RFC 4412 priority.
async fn invite(s: &B2buaScene, class: Class) -> ClientInvite {
    let invite = s.alice.invite(&s.bob).with_sdp(OFFER_SDP);
    let invite = if class == Class::Emergency {
        invite.with_header("Resource-Priority", "esnet.0")
    } else {
        invite
    };
    invite.through(s.b2bua.addr).send().await
}

/// The start lines of every response the b2bua sent `to`, from the recording.
fn responses_to(s: &B2buaScene, to: &Agent) -> Vec<String> {
    s.h.wire_entries()
        .into_iter()
        .filter(|e| e.from == s.b2bua.addr && e.to == to.addr() && e.raw.starts_with(b"SIP/2.0 "))
        .map(|e| String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string())
        .collect()
}

/// How many INVITE datagrams `from` sent the b2bua, from the recording.
fn invites_from(s: &B2buaScene, from: &Agent) -> usize {
    s.h.wire_entries()
        .iter()
        .filter(|e| e.from == from.addr() && e.to == s.b2bua.addr && e.raw.starts_with(b"INVITE "))
        .count()
}

/// The router is wedged from before the INVITE until `wedged` after its send:
/// alice sends her INVITE and its Timer A retransmissions due in that span,
/// and hears nothing back, the 100 included. Then the router runs free.
async fn wedged_for(
    s: &B2buaScene,
    pace: &RouterPace,
    class: Class,
    wedged: Duration,
) -> ClientInvite {
    pace.set(NODE, Pace::Frozen);
    let call = invite(s, class).await;
    let mut due = T1;
    let mut elapsed = Duration::ZERO;
    let mut sent = 1;
    while elapsed + due < wedged {
        s.h.advance(due).await;
        elapsed += due;
        call.retransmit().await;
        sent += 1;
        due *= 2;
    }
    s.h.advance(wedged - elapsed).await;
    assert_eq!(invites_from(s, &s.alice), sent, "the INVITE and its retransmissions");
    assert_eq!(responses_to(s, &s.alice), Vec::<String>::new(), "no 100 before the turn starts");
    pace.set(NODE, Pace::Free);
    call
}

/// The callee answers, the caller ACKs and hangs up.
async fn answer_and_hang_up(call: &mut ClientInvite, bob: &Agent) {
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    hangup(&mut dialog, bob).await;
}

/// A turn that starts inside the start bound sends the 100 it withheld, and the
/// call is set up and ends as any other.
async fn inside_the_start_bound(name: &str, class: Class) {
    let pace = RouterPace::new();
    let s = scene(name, &pace).await;
    let mut call = wedged_for(&s, &pace, class, WEDGED_INSIDE).await;

    s.h.advance(TRANSIT * 2).await;
    call.expect(100).await;
    answer_and_hang_up(&mut call, &s.bob).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert_eq!(responses_to(&s, &s.alice).first().map(String::as_str), Some("SIP/2.0 100 Trying"));
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(class), 1, "an accepted call");
    assert_eq!(counts.rejected_sum(), 0);
    let _ = s.finish().await;
}

/// A turn that starts past the start bound dials no one: its caller, who never
/// heard a 100, gets the bare 503 of a refused new call, its only response, and
/// the call writes the CDR of a call no decision reached.
async fn past_the_start_bound(name: &str, class: Class) {
    let pace = RouterPace::new();
    let s = scene(name, &pace).await;
    let mut call = wedged_for(&s, &pace, class, WEDGED_PAST).await;

    s.h.advance(TRANSIT * 2).await;
    let refused = call.expect(503).await;
    assert_eq!(refused.reason(), "Service Unavailable");
    assert!(!refused.has(&HeaderName::Reason), "no Reason");
    assert!(!refused.has(&HeaderName::RetryAfter), "no Retry-After");
    assert_eq!(responses_to(&s, &s.alice), ["SIP/2.0 503 Service Unavailable"], "one response");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    assert!(s.bob.take_queued().await.is_none(), "no call dialled");

    let cdrs = s.b2bua.cdr_records();
    let cdr = &cdrs[0];
    assert_eq!(cdr.a_leg.call_id, call.call_id());
    assert!(cdr.b_legs.is_empty(), "no b-leg: {cdr:?}");
    assert!(cdr.decision_log.is_empty(), "no decision: {cdr:?}");
    assert_eq!(cdr.termination.as_ref().map(|t| t.cause), Some(TerminationCause::Admission));
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(class), 0, "not an accepted call");
    assert_eq!(counts.rejected(Refusal::StartBound, class), 1, "refused at its start bound");
    assert_eq!(counts.rejected(Refusal::SetupBudget, class), 0, "inside its setup budget");
    assert_eq!(counts.rejected_sum(), 1, "a refused new call, counted once");
    let _ = s.finish().await;
}

/// A re-INVITE keeps the §17.2.1 bound from its receipt: behind a wedged router
/// its 100 leaves 200 ms after it arrived, and once the router runs the
/// re-INVITE is relayed and answered as any other.
#[tokio::test(start_paused = true)]
async fn a_reinvite_behind_a_wedged_router_hears_its_100_at_the_bound() {
    let pace = RouterPace::new();
    let s = scene("trying-reinvite-at-the-bound", &pace).await;
    let mut dialog = s.establish().await;
    pace.set(NODE, Pace::Frozen);
    let mut reinvite = dialog.reinvite(Some(OFFER_SDP)).await;
    let cseq = dialog.local_cseq();
    s.h.advance(TRANSIT + TRYING_BOUND - MARGIN).await;
    assert!(s.alice.take_queued().await.is_none(), "no 100 before the bound");
    s.h.advance(MARGIN * 2 + TRANSIT).await;
    reinvite.expect(100).await;

    pace.set(NODE, Pace::Free);
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    reinvite.expect(200).await;
    dialog.ack_for(cseq, None).await;
    s.bob.receive("ACK").await;
    hangup(&mut dialog, &s.bob).await;
    let _ = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_inside_the_start_bound_sends_the_withheld_100() {
    inside_the_start_bound("trying-inside-the-start-bound", Class::Normal).await;
}

#[tokio::test(start_paused = true)]
async fn an_emergency_turn_inside_the_start_bound_sends_the_withheld_100() {
    inside_the_start_bound("trying-emergency-inside-the-start-bound", Class::Emergency).await;
}

#[tokio::test(start_paused = true)]
async fn a_turn_past_the_start_bound_answers_a_bare_503_and_dials_no_one() {
    past_the_start_bound("trying-past-the-start-bound", Class::Normal).await;
}

#[tokio::test(start_paused = true)]
async fn an_emergency_turn_past_the_start_bound_answers_a_bare_503_and_dials_no_one() {
    past_the_start_bound("trying-emergency-past-the-start-bound", Class::Emergency).await;
}
