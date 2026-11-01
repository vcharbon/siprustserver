//! The router takes the keepalive lane first, in the lane's order, and never
//! starves the rest: out-of-dialog OPTIONS queued behind a held turn are
//! answered, and the dead notices they carry read, in the order they arrived
//! (ADR-0050 §12); under a steady stream of OPTIONS, an event waiting in the
//! event queue is still taken within a turn or two.
//!
//! The router loop is held by [`RouterPace`]; the event-queue work is
//! out-of-dialog requests the worker refuses `405`. An established call
//! outlives each case and ends by its BYE.

use std::time::Duration;

use b2bua::TurnProbes;
use b2bua_harness::{
    cluster_key, drop_backlog_answers, send_backlog, B2buaScene, B2buaSut, Pace, RouterPace,
    BACKLOG_METHOD,
};
use scenario_harness::Agent;
use sip_message::dead_notice::{DeadNotice, X_WORKER_DEAD};
use sip_message::generators::OutOfDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use tokio::time::Instant;

/// The single SUT's ordinal.
const NODE: &str = "w0";
/// Past a datagram's flight through the harness's fabric.
const TRANSIT: Duration = Duration::from_millis(150);

/// A scene whose worker's router loop is paced by the returned handle and
/// trusts the harness's cluster key, with carol, the sender of event-queue
/// work.
async fn paced_scene(name: &str) -> (B2buaScene, std::sync::Arc<RouterPace>, Agent) {
    let pace = RouterPace::new();
    let probe = TurnProbes::new(pace.clone());
    let s = B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port)
            .turn_probe(probe)
            .tune(|c| c.cluster_key = Some(cluster_key()))
    })
    .await;
    let carol = s.h.agent_with_pre_ingress("carol", "127.0.0.1:5062", drop_backlog_answers()).await;
    (s, pace, carol)
}

/// The `X-Worker-Dead` value of a verdict on `ordinal` at `at_ms`, signed
/// under the cluster key.
fn notice(ordinal: &str, at_ms: i64) -> String {
    let notice = DeadNotice { ordinal: ordinal.to_string(), at_ms };
    let fields = notice.signed_fields();
    let signature = cluster_key().sign(&fields.each_ref().map(Vec::as_slice));
    notice.header_value(&signature).expect("a token ordinal")
}

/// Two OPTIONS carrying two verdicts reach the lane while the router is held
/// in a turn; released, it answers them, and reads their notices right after
/// each answer, in the order they arrived. Both notices verify.
#[tokio::test(start_paused = true)]
async fn options_held_behind_a_turn_are_taken_in_arrival_order() {
    let (s, pace, carol) = paced_scene("keepalive-lane-arrival-order").await;
    let mut dialog = s.establish().await;

    pace.set(NODE, Pace::Frozen);
    send_backlog(&carol, s.b2bua.addr, 1, "held").await;
    s.h.advance(TRANSIT).await;
    let now = s.b2bua.clock().now_ms();
    let mut first = s
        .alice
        .request(OutOfDialogMethod::Options, &s.bob)
        .with_header(X_WORKER_DEAD, &notice("w8", now))
        .through(s.b2bua.addr)
        .send()
        .await;
    s.h.advance(Duration::from_millis(10)).await;
    let mut second = s
        .alice
        .request(OutOfDialogMethod::Options, &s.bob)
        .with_header(X_WORKER_DEAD, &notice("w9", now + 10))
        .through(s.b2bua.addr)
        .send()
        .await;
    s.h.advance(TRANSIT).await;
    pace.set(NODE, Pace::Free);
    let first_id = first.expect(200).await.call_id().as_str().to_string();
    let second_id = second.expect(200).await.call_id().as_str().to_string();
    assert_ne!(first_id, second_id);
    assert_eq!(s.b2bua.metrics().dead_notices_refused_total(), 0, "both notices verify");

    s.hangup(&mut dialog).await;
    let report = s.finish().await;
    let answered: Vec<String> = report
        .entries()
        .iter()
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() == 200 && *r.cseq().method() == Method::Options =>
            {
                Some(r.call_id().as_str().to_string())
            }
            _ => None,
        })
        .filter(|id| *id == first_id || *id == second_id)
        .collect();
    assert_eq!(answered.first(), Some(&first_id), "the first to arrive is answered first");
    assert_eq!(answered.last(), Some(&second_id), "the second after it");
}

/// OPTIONS arrive every 5 ms while each router turn costs 10 ms, so the lane
/// is never empty; a request waiting in the event queue is still taken
/// within a few turns, while the stream goes on.
#[tokio::test(start_paused = true)]
async fn a_stream_of_options_starves_no_queued_event() {
    let (s, pace, carol) = paced_scene("keepalive-lane-fairness").await;
    let mut dialog = s.establish().await;
    let refused = || s.b2bua.metrics().unroutable_refused_of(BACKLOG_METHOD, 405);

    pace.set(NODE, Pace::PerTurn(Duration::from_millis(10)));
    let mut probes = Vec::new();
    let mut queued_at = None;
    for i in 0..80 {
        let options =
            s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
        probes.push(options);
        if i == 5 {
            send_backlog(&carol, s.b2bua.addr, 1, "queued").await;
            queued_at = Some(Instant::now() + TRANSIT);
        }
        s.h.advance(Duration::from_millis(5)).await;
    }
    let taken_by = queued_at.expect("sent") + Duration::from_millis(50);
    assert!(Instant::now() > taken_by, "the stream outlasts the bound");
    assert_eq!(refused(), 1, "the queued request is taken while OPTIONS keep arriving");

    pace.set(NODE, Pace::Free);
    for mut options in probes {
        options.expect(200).await;
    }
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}
