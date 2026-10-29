//! The requests a call awaits answers to over a wired core: a dead verdict
//! takes over a call whose only work is such a request and sends it again;
//! the answer to a request a node sent never takes a call over.

use std::time::Duration;

use call::{Call, LegState, OffTurnRequest};
use sip_net_testkit::SimulatedSignalingNetwork;

use b2bua_sdk::event::CallEvent;

use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::repl::ReplicatingCallStore;
use crate::router::test_support::{invite, node_on, src};
use crate::store::{CallStore, PartitionRole, PutOpts};

/// A call `w0` owns and `w1` backs up, answered, awaiting a service's HTTP
/// request's answer and nothing else.
fn awaiting(cid: &str) -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", cid),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    *call.a_leg.invite_final_sent_mut() = Some(200);
    call.a_leg.own_tag = Some("b2b-tag".into());
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
    call.awaited.push(OffTurnRequest::ServiceHttp {
        correlation_id: "svc:ask".into(),
        endpoint: "/ask".into(),
        method: "POST".into(),
        headers: vec![],
        body: b"q".to_vec(),
        content_type: None,
        timeout_ms: None,
    });
    call
}

async fn put(store: &ReplicatingCallStore, call: &Call) {
    let body = crate::store::envelope::placed_body(call).unwrap();
    store
        .put_call(
            PartitionRole::Backup,
            "w0",
            &call.call_ref,
            body,
            &[],
            60_000,
            1,
            0,
            &PutOpts::default(),
        )
        .await
        .unwrap();
}

/// A call whose only work is an awaited request is taken over at the dead
/// verdict and sends the request again; with no adaptation port its answer
/// comes at once and takes it off the record.
#[tokio::test(start_paused = true)]
async fn a_call_awaiting_an_answer_is_taken_over_at_the_dead_verdict_and_asks_again() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let ctx = n.core.router_ctx();
    let call = awaiting("awaiting");
    put(&n.store, &call).await;

    assert!(super::super::departure::take_over_read_dead(ctx, "w0"));
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 1, "the call was taken over");
    let body = n.store.get_call(PartitionRole::Backup, "w0", &call.call_ref).await.unwrap();
    let element = crate::store::envelope::decode(&call::MsgpackCodec::new(), &body.unwrap());
    assert!(
        element.unwrap().call.awaited.is_empty(),
        "the copy sent the request again and took its answer"
    );
    assert!(ctx.state.peek(&call.call_ref).is_none(), "then let the call go");
}

/// The answer to a request a node sent for a call it no longer holds (a copy
/// stood down, its request sent again) reaches no call and takes none over.
#[tokio::test(start_paused = true)]
async fn an_answer_for_a_call_not_held_here_takes_nothing_over() {
    let net = SimulatedSignalingNetwork::new(1);
    let n = node_on(&net, "w1", |_| {}, Vec::new()).await;
    let ctx = n.core.router_ctx();
    let call = awaiting("stood-down");
    put(&n.store, &call).await;

    let answer = CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: "service-http-result".into(),
        outcome: "ok".into(),
        payload: serde_json::json!({ "correlation_id": "svc:ask", "status": 200 }),
        body: Vec::new(),
        incarnation: Some(call.incarnation().to_string()),
    };
    ctx.reentry_tx.send(answer).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(n.metrics.repl_takeover_hydrated_total(), 0, "nothing was taken over");
    assert!(ctx.state.peek(&call.call_ref).is_none(), "the call is not served here");
}
