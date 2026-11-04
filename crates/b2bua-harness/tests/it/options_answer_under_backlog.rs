//! A worker answers the out-of-dialog OPTIONS its front proxy probes it with
//! ahead of the events queued for its router: with a backlog that takes the
//! router seconds to work through, the answer arrives within the probe's
//! reply window, and the backlog is still queued when it does. The backlog is
//! out-of-dialog requests the worker refuses `405`, behind a router loop that
//! spends [`PER_TURN`] of simulated time on each turn ([`RouterPace`]); an
//! established call outlives it and ends by its BYE.

use std::time::Duration;

use b2bua::TurnProbes;
use b2bua_harness::{
    drop_backlog_answers, send_backlog, B2buaScene, B2buaSut, Pace, RouterPace, BACKLOG_METHOD,
};
use sip_message::generators::OutOfDialogMethod;
use tokio::time::Instant;

/// The single SUT's ordinal.
const NODE: &str = "w0";
/// The simulated time each router turn costs.
const PER_TURN: Duration = Duration::from_millis(10);
/// Events queued ahead of the OPTIONS: three seconds of router turns.
const BACKLOG: usize = 300;
/// The proxy probe's reply window (`timeout_ms`).
const REPLY_WINDOW: Duration = Duration::from_millis(1500);

#[tokio::test(start_paused = true)]
async fn an_options_behind_a_backlog_is_answered_within_the_probe_reply_window() {
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let s = B2buaScene::with_b2bua("options-answer-under-backlog", move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).turn_probe(probe)
    })
    .await;
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", drop_backlog_answers()).await;
    let mut dialog = s.establish().await;

    pace.set(NODE, Pace::PerTurn(PER_TURN));
    send_backlog(&carol, s.b2bua.addr, BACKLOG, "backlog").await;
    let sent = Instant::now();
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    options.expect(200).await;
    let waited = sent.elapsed();
    let queued = s.b2bua.txn_metrics().event_queue_depth();
    assert!(
        waited <= REPLY_WINDOW,
        "the OPTIONS is answered within the probe's reply window, not after the backlog: \
         answered after {waited:?}"
    );
    assert!(queued >= BACKLOG / 2, "the backlog is still queued at the answer: {queued} events");

    let refused = || s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);
    let give_up = Instant::now() + Duration::from_secs(10);
    while refused() < BACKLOG as u64 && Instant::now() < give_up {
        s.h.advance(Duration::from_millis(250)).await;
    }
    assert_eq!(refused(), BACKLOG as u64, "every backlog request is refused 405");
    pace.set(NODE, Pace::Free);
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
