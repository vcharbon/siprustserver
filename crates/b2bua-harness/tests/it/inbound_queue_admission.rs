//! A backlog in the worker's inbound UDP queue, ahead of the transaction
//! layer, sheds new calls as one in the event queue does (ADR-0050 §9): once
//! the queue's oldest datagram has waited the gate's refusal delay through its
//! interval, 450 ms, the gate refuses and a new normal INVITE draws the
//! new-call `503` with a `Retry-After`. With the ingress brake installed the
//! refusal is made as the INVITE arrives, ahead of the queue; without it, by
//! the transaction layer when it takes the INVITE. An emergency INVITE and an
//! in-dialog request are served, and the worker accepts again once the queue
//! has stayed drained through the hold.
//!
//! The backlog is out-of-dialog requests the worker refuses `405`, behind a
//! transaction layer that takes one datagram per [`PER_TAKE`] of simulated
//! time ([`InboundPace`]); the gate runs on its defaults.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::ingress_brake::IngressBrakeConfig;
use b2bua::new_calls::Refusal;
use b2bua_harness::{
    drop_backlog_answers, send_backlog, settle_until, B2buaScene, B2buaSut, InboundPace,
    BACKLOG_METHOD,
};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::{InDialogMethod, OutOfDialogMethod};
use sip_message::header::{Reason, RetryAfter};
use sip_message::worker_report::{self, Readiness};
use tokio::time::Instant;

/// The simulated time between two takes of the transaction layer.
const PER_TAKE: Duration = Duration::from_millis(10);
/// Datagrams queued: two seconds of takes, below the bind's 256.
const BACKLOG: usize = 200;
/// The wait of the oldest queued datagram at which the gate refuses: its
/// refusal delay and interval together.
const REFUSE_AT_WAIT: Duration = Duration::from_millis(450);
/// The takes that pass while an INVITE and its answer are in flight: the
/// fabric's 100 ms each way.
const FLIGHT_TAKES: u64 = 20;
/// How long the gate stays refusing once the queue has drained.
const ACCEPT_HOLD: Duration = Duration::from_millis(3000);
/// The re-INVITE's offer.
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// A scene whose worker takes its datagrams at the returned pace, with the
/// ingress brake on the wait alone when `brake`, and carol, the backlog
/// sender.
async fn paced_scene(name: &str, brake: bool) -> (B2buaScene, InboundPace, Agent) {
    let pace = InboundPace::new();
    let inbound = pace.clone();
    let s = B2buaScene::with_b2bua(name, move |bob_port| {
        let sut = B2buaSut::route_all_to("127.0.0.1", bob_port).inbound_pace(inbound);
        if brake {
            sut.ingress_brake(IngressBrakeConfig::by_depth(256, 100))
        } else {
            sut
        }
    })
    .await;
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", drop_backlog_answers()).await;
    (s, pace, carol)
}

/// Whether the gate refuses now, as the worker's metrics state it.
fn gate_refusing(s: &B2buaScene) -> bool {
    s.b2bua.readiness().prometheus_text().lines().any(|l| l == "b2bua_event_queue_refusing 1")
}

/// The backlog requests the worker has refused `405`.
fn taken(s: &B2buaScene) -> u64 {
    s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405)
}

/// Pump until the worker has refused the whole backlog: the instant its
/// inbound queue drained.
async fn drained(s: &B2buaScene) -> Instant {
    let give_up = Instant::now() + Duration::from_secs(30);
    while taken(s) < BACKLOG as u64 && Instant::now() < give_up {
        s.h.advance(PER_TAKE).await;
    }
    assert_eq!(taken(s), BACKLOG as u64, "every backlog request is refused 405");
    Instant::now()
}

/// Pump, a take at a time, until the gate refuses: the wait of the inbound
/// queue's oldest datagram at that reading.
async fn wait_at_the_trip(s: &B2buaScene) -> Duration {
    let give_up = Instant::now() + Duration::from_secs(2);
    while !gate_refusing(s) && Instant::now() < give_up {
        s.h.advance(PER_TAKE).await;
    }
    assert!(gate_refusing(s), "the inbound queue's wait trips the gate");
    s.b2bua.txn_metrics().inbound_queue_delay()
}

/// A new normal INVITE from alice, refused `503` with a `Retry-After` and no
/// `Reason`.
async fn refused(s: &B2buaScene) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let resp = call.expect(503).await;
    assert!(resp.header::<RetryAfter>().is_some(), "the new-call 503 carries a Retry-After");
    assert!(
        resp.list::<Reason>().expect("readable Reason").is_empty(),
        "the new-call 503 carries no Reason"
    );
}

/// A new INVITE from alice, `emergency` or not, admitted, answered by bob and
/// confirmed.
async fn admitted(s: &B2buaScene, emergency: bool) -> Dialog {
    let invite = s.alice.invite(&s.bob).with_sdp(OFFER_SDP);
    let invite =
        if emergency { invite.with_header("Resource-Priority", "esnet.0") } else { invite };
    let mut call = invite.through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    dialog
}

/// A re-INVITE on `dialog`, answered by bob and confirmed.
async fn reinvited(s: &B2buaScene, dialog: &mut Dialog) {
    let mut round = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = s.bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    round.expect(200).await;
    dialog.ack(None).await;
    s.bob.receive("ACK").await;
}

/// The acceptance alice's out-of-dialog OPTIONS reads, answered `200`.
async fn readiness(s: &B2buaScene) -> Option<Readiness> {
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let resp = options.expect(200).await;
    worker_report::readiness(&resp)
}

/// The gate trips on the inbound queue's wait at 450 ms, a turn or two late
/// at most, and not before; the brake then refuses a new INVITE as it
/// arrives, ahead of the queued backlog. Inside the backlog and the hold an
/// emergency INVITE and a re-INVITE are served; after the hold a new INVITE
/// is admitted.
#[tokio::test(start_paused = true)]
async fn a_backlog_in_the_inbound_queue_trips_the_gate_and_the_brake_refuses_on_arrival() {
    let (s, pace, carol) = paced_scene("inbound-queue-admission-brake", true).await;
    let mut held = s.establish().await;

    pace.set(PER_TAKE);
    send_backlog(&carol, s.b2bua.addr, BACKLOG, "backlog").await;
    let tripped_at = wait_at_the_trip(&s).await;
    assert!(
        (REFUSE_AT_WAIT..=REFUSE_AT_WAIT + 3 * PER_TAKE).contains(&tripped_at),
        "the gate tripped with the oldest datagram waiting {tripped_at:?}"
    );

    let before = taken(&s);
    refused(&s).await;
    assert!(
        taken(&s) - before <= FLIGHT_TAKES + 2,
        "refused on arrival, not behind the queued backlog: {} taken meanwhile",
        taken(&s) - before
    );
    assert!(taken(&s) < BACKLOG as u64 / 2, "half the backlog still waits");

    let before = taken(&s);
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "the probe's answer states the refusal"
    );
    assert!(
        taken(&s) - before <= FLIGHT_TAKES + 2,
        "the probe is answered ahead of the queued backlog: {} taken meanwhile",
        taken(&s) - before
    );

    reinvited(&s, &mut held).await;
    let drained = drained(&s).await;
    pace.set(Duration::ZERO);

    assert_eq!(readiness(&s).await, Some(Readiness::refuse("overload")));
    let mut emergency = admitted(&s, true).await;
    refused(&s).await;
    assert!(drained.elapsed() < ACCEPT_HOLD, "all of it inside the hold");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::EventQueue, Class::Normal), 2, "each counted once");
    assert_eq!(counts.rejected_sum(), 2, "under the gate's reason alone");
    let brake = s.b2bua.ingress_brake().expect("the brake is installed");
    assert_eq!(brake.gate_refused(Class::Normal), 2, "both made at the brake, on arrival");
    assert_eq!(brake.emergency_bypassed(), 0, "the queue never reached its threshold");

    s.h.advance(ACCEPT_HOLD).await;
    assert_eq!(readiness(&s).await, Some(Readiness::accept()));
    let mut fresh = admitted(&s, false).await;

    s.hangup(&mut fresh).await;
    s.hangup(&mut emergency).await;
    s.hangup(&mut held).await;
    settle_until(|| s.b2bua.cdr_records().len() == 3).await;
    let _report = s.finish().await;
}

/// With no brake on the bind, a new INVITE waits its turn in the inbound
/// queue and the transaction layer refuses it when it takes it, under the
/// event-queue reason: the gate tripped on the queue's wait and refuses.
#[tokio::test(start_paused = true)]
async fn with_no_brake_the_transaction_layer_refuses_while_the_gate_does() {
    let (s, pace, carol) = paced_scene("inbound-queue-admission-no-brake", false).await;
    let mut held = s.establish().await;

    pace.set(PER_TAKE);
    send_backlog(&carol, s.b2bua.addr, BACKLOG, "backlog").await;
    wait_at_the_trip(&s).await;
    refused(&s).await;
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::EventQueue, Class::Normal), 1);
    assert_eq!(counts.rejected_sum(), 1, "under the event-queue reason alone");
    assert!(s.b2bua.cdr_records().is_empty(), "a ladder refusal writes no call record");

    drained(&s).await;
    pace.set(Duration::ZERO);
    s.h.advance(ACCEPT_HOLD).await;
    let mut fresh = admitted(&s, false).await;

    s.hangup(&mut fresh).await;
    s.hangup(&mut held).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    let _report = s.finish().await;
}
