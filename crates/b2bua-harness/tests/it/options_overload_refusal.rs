//! A worker whose event queue is backed up refuses new calls on its OPTIONS
//! answer (`new=refuse; cause=overload`, ADR-0050 §2), by the queue's depth or
//! by how long its events wait, and accepts again only once the queue has
//! stayed drained for the hold: a dip shorter than the hold flips nothing.
//!
//! The backlog is out-of-dialog requests the worker refuses `405`, behind a
//! router loop that spends simulated time on each turn ([`RouterPace`]). The
//! thresholds are the worker's defaults; an established call outlives every
//! backlog and ends by its BYE.

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
/// Queued events at which the worker refuses new calls.
const REFUSE_DEPTH: usize = 2048;
/// A wait in the queue at which the worker refuses new calls.
const REFUSE_SOJOURN: Duration = Duration::from_millis(500);
/// How long the queue stays drained before the worker accepts again.
const ACCEPT_HOLD: Duration = Duration::from_millis(3000);

/// A scene whose worker's router loop is paced by the returned handle, with
/// carol, the backlog sender.
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

/// Pump until the worker has refused `n` backlog requests in all: the
/// instant its router took the last of them.
async fn drained(s: &B2buaScene, n: u64) -> Instant {
    let refused = || s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);
    let give_up = Instant::now() + Duration::from_secs(30);
    while refused() < n && Instant::now() < give_up {
        s.h.advance(Duration::from_millis(10)).await;
    }
    assert_eq!(refused(), n, "every backlog request is refused 405");
    Instant::now()
}

/// Pump until `at`.
async fn advance_to(s: &B2buaScene, at: Instant) {
    s.h.advance(at.saturating_duration_since(Instant::now())).await;
}

/// Events that have waited past the sojourn threshold refuse new calls while
/// the queue is far below the depth threshold. Once drained the worker keeps
/// refusing through the hold; a second, smaller backlog inside the hold, whose
/// events wait above the accept bound and below the refusal one, restarts it.
#[tokio::test(start_paused = true)]
async fn events_waiting_past_the_sojourn_threshold_refuse_new_calls_until_the_hold_passes() {
    let (s, pace, carol) = paced_scene("options-overload-refusal-sojourn").await;
    let mut dialog = s.establish().await;
    assert_eq!(readiness(&s).await, Some(Readiness::accept()), "an idle worker accepts");

    // 300 events at 10 ms a turn: three seconds of work, far below the depth.
    pace.set(NODE, Pace::PerTurn(Duration::from_millis(10)));
    send_backlog(&carol, s.b2bua.addr, 300, "backlog").await;
    s.h.advance(2 * REFUSE_SOJOURN).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "events waiting past the sojourn threshold refuse new calls"
    );

    let first_drain = drained(&s, 300).await;
    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "drained for less than the hold: still refusing"
    );

    // 20 events: the last waits about 200 ms, between the two sojourn bounds.
    send_backlog(&carol, s.b2bua.addr, 20, "bump").await;
    let second_drain = drained(&s, 320).await;
    advance_to(&s, first_drain + ACCEPT_HOLD + Duration::from_millis(500)).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "a dip inside the hold flips nothing: the bump restarted the hold"
    );

    advance_to(&s, second_drain + ACCEPT_HOLD + Duration::from_millis(500)).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::accept()),
        "drained through the whole hold: accepting again"
    );
    pace.set(NODE, Pace::Free);
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// A burst past the depth threshold refuses new calls at once, before any of
/// its events has waited the sojourn threshold, and the worker accepts again
/// once the queue has stayed drained for the hold.
#[tokio::test(start_paused = true)]
async fn a_queue_past_the_depth_threshold_refuses_new_calls_until_the_hold_passes() {
    let (s, pace, carol) = paced_scene("options-overload-refusal-depth").await;
    let mut dialog = s.establish().await;

    // Past the threshold by what the router takes while the OPTIONS travels.
    let burst = REFUSE_DEPTH + 256;
    pace.set(NODE, Pace::PerTurn(Duration::from_millis(1)));
    let sent = Instant::now();
    send_backlog(&carol, s.b2bua.addr, burst, "backlog").await;
    let mut queued = 0;
    while queued < REFUSE_DEPTH && sent.elapsed() < REFUSE_SOJOURN {
        s.h.advance(Duration::from_millis(5)).await;
        queued = s.b2bua.txn_metrics().event_queue_depth();
    }
    assert!(queued >= REFUSE_DEPTH, "the burst is queued: {queued} events");
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "a queue past the depth threshold refuses new calls"
    );
    assert!(
        sent.elapsed() < REFUSE_SOJOURN,
        "refused on the depth alone, before an event waited the sojourn threshold: {:?}",
        sent.elapsed()
    );

    let drain = drained(&s, burst as u64).await;
    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::refuse("overload")),
        "drained for less than the hold: still refusing"
    );
    advance_to(&s, drain + ACCEPT_HOLD + Duration::from_millis(500)).await;
    assert_eq!(
        readiness(&s).await,
        Some(Readiness::accept()),
        "drained through the whole hold: accepting again"
    );
    pace.set(NODE, Pace::Free);
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
