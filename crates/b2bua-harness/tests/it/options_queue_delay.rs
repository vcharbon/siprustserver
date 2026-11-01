//! The event-queue gate reads the age of the oldest event waiting in the
//! queue at the instant it reads, and refuses on that delay only once it has
//! stayed at or past the refusal bound for the persistence interval
//! (ADR-0050 §9): a router that takes nothing still reads a growing delay,
//! and one slow turn shorter than the refusal delay plus the interval refuses
//! nothing.
//!
//! The waiting events are out-of-dialog requests the worker refuses `405`,
//! behind a router loop held by [`RouterPace`]; the gate runs on its defaults
//! (delay 300 ms, interval 150 ms). An established call outlives each case and
//! ends by its BYE.

use std::time::Duration;

use b2bua::TurnProbes;
use b2bua_harness::{
    drop_backlog_answers, send_backlog, B2buaScene, B2buaSut, Pace, RouterPace, BACKLOG_METHOD,
};
use scenario_harness::Agent;
use sip_message::generators::OutOfDialogMethod;
use sip_message::worker_report::{self, Readiness};
use tokio::time::Instant;

/// The single SUT's ordinal.
const NODE: &str = "w0";
/// The delay at which the gate refuses, once it has persisted.
const REFUSE_DELAY: Duration = Duration::from_millis(300);
/// How long the delay persists before the gate refuses.
const REFUSE_INTERVAL: Duration = Duration::from_millis(150);
/// Past a datagram's flight through the harness's fabric.
const TRANSIT: Duration = Duration::from_millis(150);

/// A scene whose worker's router loop is paced by the returned handle, with
/// carol, the sender of the waiting events.
async fn paced_scene(name: &str) -> (B2buaScene, std::sync::Arc<RouterPace>, Agent) {
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let s = B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).turn_probe(probe)
    })
    .await;
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", drop_backlog_answers()).await;
    (s, pace, carol)
}

/// The acceptance alice's out-of-dialog OPTIONS reads, answered `200`.
async fn readiness(s: &B2buaScene) -> Option<Readiness> {
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let resp = options.expect(200).await;
    worker_report::readiness(&resp)
}

/// Pump until the worker has refused `n` waiting requests in all.
async fn drained(s: &B2buaScene, n: u64) {
    let refused = || s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);
    let give_up = Instant::now() + Duration::from_secs(10);
    while refused() < n && Instant::now() < give_up {
        s.h.advance(Duration::from_millis(10)).await;
    }
    assert_eq!(refused(), n, "every waiting request is refused 405");
}

/// A router held in one turn takes nothing; the event queued behind that turn
/// ages, and the OPTIONS the router takes next, ahead of it, reads its age:
/// the worker refuses new calls although the last event it took waited
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_stalled_router_reads_the_age_of_the_event_waiting_behind_it() {
    let (s, pace, carol) = paced_scene("options-queue-delay-stalled").await;
    let mut dialog = s.establish().await;

    pace.set(NODE, Pace::Frozen);
    send_backlog(&carol, s.b2bua.addr, 1, "taken").await;
    s.h.advance(TRANSIT).await;
    send_backlog(&carol, s.b2bua.addr, 1, "waiting").await;
    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(s.b2bua.txn_metrics().event_queue_depth(), 1, "one event waits behind the turn");

    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    s.h.advance(TRANSIT).await;
    pace.set(NODE, Pace::Free);
    let resp = options.expect(200).await;
    assert_eq!(
        worker_report::readiness(&resp),
        Some(Readiness::refuse("overload")),
        "the event waiting a second behind a stalled router refuses new calls"
    );

    drained(&s, 2).await;
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// One slow turn holds an event behind it past the refusal bound, but for
/// less than the persistence interval: the OPTIONS that arrives during the
/// turn, read right after it, and the one after the event is taken both
/// accept.
#[tokio::test(start_paused = true)]
async fn a_wait_shorter_than_the_persistence_interval_refuses_nothing() {
    let (s, pace, carol) = paced_scene("options-queue-delay-spike").await;
    let mut dialog = s.establish().await;
    let slow = REFUSE_DELAY + REFUSE_INTERVAL / 2;

    pace.set(NODE, Pace::PerTurn(slow));
    send_backlog(&carol, s.b2bua.addr, 1, "slow").await;
    send_backlog(&carol, s.b2bua.addr, 1, "behind").await;
    s.h.advance(TRANSIT).await;
    pace.set(NODE, Pace::Free);
    // Both arrived at 100 ms; the slow turn runs to 475 ms. Sent at 320 ms,
    // the OPTIONS arrives at 420 ms, the event behind 320 ms old.
    s.h.advance(Duration::from_millis(170)).await;
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let during = worker_report::readiness(&options.expect(200).await);
    assert_eq!(during, Some(Readiness::accept()), "read past the bound, inside the interval");

    drained(&s, 2).await;
    assert_eq!(readiness(&s).await, Some(Readiness::accept()), "nothing latched a refusal");
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// Turns slow enough that the waiting events' delay stays past the refusal
/// bound for longer than the interval refuse new calls.
#[tokio::test(start_paused = true)]
async fn a_delay_sustained_past_the_persistence_interval_refuses() {
    let (s, pace, carol) = paced_scene("options-queue-delay-sustained").await;
    let mut dialog = s.establish().await;

    pace.set(NODE, Pace::PerTurn(Duration::from_millis(100)));
    send_backlog(&carol, s.b2bua.addr, 20, "sustained").await;
    s.h.advance(REFUSE_DELAY + 2 * REFUSE_INTERVAL).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "a delay held past the bound for the interval refuses new calls"
    );

    drained(&s, 20).await;
    pace.set(NODE, Pace::Free);
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
