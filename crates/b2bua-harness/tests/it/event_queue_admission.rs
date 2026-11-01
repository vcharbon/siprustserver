//! The event-queue rung of the admission ladder (ADR-0037): while the
//! event-queue gate refuses (ADR-0050 §9), a new normal INVITE is refused at
//! router ingress with the new-call `503` and its `Retry-After`, from the
//! instant the gate trips and with no probe needed. An emergency INVITE and
//! an in-dialog request are judged as by the panic-ELU rung: admitted. Once
//! the gate accepts again, a new INVITE is admitted.
//!
//! The backlog is out-of-dialog requests the worker refuses `405`, behind a
//! router loop that spends [`PER_TURN`] of simulated time on each turn
//! ([`RouterPace`]); the gate runs on its defaults (delay 300 ms held 150 ms,
//! hold 3 s).
//! No OPTIONS is sent: the rung reads the gate itself.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::new_calls::Refusal;
use b2bua::TurnProbes;
use b2bua_harness::{
    drop_backlog_answers, send_backlog, settle_until, B2buaScene, B2buaSut, Pace, RouterPace,
    BACKLOG_METHOD,
};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::header::{Reason, RetryAfter};
use tokio::time::Instant;

/// The single SUT's ordinal.
const NODE: &str = "w0";
/// The simulated time each router turn costs while the backlog queues.
const PER_TURN: Duration = Duration::from_millis(10);
/// Events queued: three seconds of router turns.
const BACKLOG: usize = 300;
/// How long the gate stays refusing once the queue has drained.
const ACCEPT_HOLD: Duration = Duration::from_millis(3000);
/// The re-INVITE's offer.
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// A new normal INVITE from alice, refused `503` with a `Retry-After` and no
/// `Reason`.
async fn refused(s: &B2buaScene) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let resp = call.expect(503).await;
    assert!(resp.header::<RetryAfter>().is_some(), "the new-call 503 carries a Retry-After");
    assert!(resp.header::<Reason>().is_none(), "the new-call 503 carries no Reason");
}

/// A new INVITE from alice, `emergency` or not, admitted, answered by bob and
/// confirmed.
async fn admitted(s: &B2buaScene, bob: &Agent, emergency: bool) -> Dialog {
    let invite = s.alice.invite(bob).with_sdp(OFFER_SDP);
    let invite =
        if emergency { invite.with_header("Resource-Priority", "esnet.0") } else { invite };
    let mut call = invite.through(s.b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    dialog
}

#[tokio::test(start_paused = true)]
async fn a_backed_up_queue_refuses_new_invites_at_the_ladder_until_the_gate_accepts() {
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let s = B2buaScene::with_b2bua("event-queue-admission", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).turn_probe(probe)
    })
    .await;
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", drop_backlog_answers()).await;
    let mut held = s.establish().await;

    // ── the queue backs up; a new INVITE behind it is refused ───────────────
    pace.set(NODE, Pace::PerTurn(PER_TURN));
    send_backlog(&carol, s.b2bua.addr, BACKLOG, "backlog").await;
    s.h.advance(Duration::from_secs(1)).await;
    refused(&s).await;

    let refused_405 = || s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);
    let give_up = Instant::now() + Duration::from_secs(10);
    while refused_405() < BACKLOG as u64 && Instant::now() < give_up {
        s.h.advance(Duration::from_millis(10)).await;
    }
    assert_eq!(refused_405(), BACKLOG as u64, "every backlog request is refused 405");
    let drained = Instant::now();
    pace.set(NODE, Pace::Free);

    // ── inside the hold: emergency and in-dialog pass, a normal INVITE not ──
    let mut emergency = admitted(&s, &s.bob, true).await;
    let mut round = held.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = s.bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    round.expect(200).await;
    held.ack(None).await;
    s.bob.receive("ACK").await;
    refused(&s).await;
    assert!(drained.elapsed() < ACCEPT_HOLD, "all of it inside the hold");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::EventQueue, Class::Normal), 2, "each counted once");
    assert_eq!(counts.rejected_sum(), 2, "under the event-queue reason alone");
    assert!(s.b2bua.cdr_records().is_empty(), "a ladder refusal writes no call record");

    // ── the hold passes: a new INVITE is admitted again ─────────────────────
    s.h.advance(ACCEPT_HOLD).await;
    let mut fresh = admitted(&s, &s.bob, false).await;

    s.hangup(&mut fresh).await;
    s.hangup(&mut emergency).await;
    s.hangup(&mut held).await;
    settle_until(|| s.b2bua.cdr_records().len() == 3).await;
    let _report = s.finish().await;
}
