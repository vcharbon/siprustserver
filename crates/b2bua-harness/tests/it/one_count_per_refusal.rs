//! A refused new INVITE moves exactly one admission counter family, once:
//! `b2bua_new_calls_total`, by one, whichever rung refused it — the ingress
//! brake, the transaction layer's backlog, a capacity ceiling, the global
//! per-call queue cap, the panic-ELU backstop or the CPS bucket. Every refused
//! INVITE here is sent once and ACKed; every admitted call is set up and torn
//! down properly.

use std::collections::BTreeMap;
use std::sync::Arc;

use b2bua::admission::Class;
use b2bua::capacity::{simulated, CapacityGate};
use b2bua::config::{CapacityConfig, Ceilings};
use b2bua::ingress_brake::IngressBrakeConfig;
use b2bua::new_calls::Refusal;
use b2bua::overload::OverloadSignal;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, OFFER_SDP};
use scenario_harness::{callflow, Agent};

const NEW_CALLS: &str = "b2bua_new_calls_total";
const EMERGENCY: (&str, &str) = ("Resource-Priority", "esnet.0");

/// The families whose count moved between two snapshots, by how much.
fn moved(
    before: &BTreeMap<&'static str, f64>,
    after: &BTreeMap<&'static str, f64>,
) -> BTreeMap<&'static str, f64> {
    after
        .iter()
        .filter_map(|(name, n)| {
            let delta = n - before.get(name).copied().unwrap_or(0.0);
            (delta != 0.0).then_some((*name, delta))
        })
        .collect()
}

/// Send a new INVITE from `caller` (emergency when asked), expect its 503,
/// and assert it moved `b2bua_new_calls_total` by one and nothing else, with
/// one more refusal of `reason` in `emergency`'s class.
async fn refused_once(
    s: &B2buaScene,
    caller: &Agent,
    emergency: bool,
    reason: Refusal,
    wait_for_count: bool,
) {
    let class = if emergency { Class::Emergency } else { Class::Normal };
    let before = s.b2bua.catalogued_counts();
    let refused_before = s.b2bua.new_calls().rejected(reason, class);
    let mut invite = caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr);
    if emergency {
        invite = invite.with_header(EMERGENCY.0, EMERGENCY.1);
    }
    let mut call = invite.send().await;
    call.expect(503).await;
    if wait_for_count {
        settle_until(|| s.b2bua.new_calls().rejected(reason, class) > refused_before).await;
    }
    let after = s.b2bua.catalogued_counts();
    assert_eq!(
        moved(&before, &after),
        BTreeMap::from([(NEW_CALLS, 1.0)]),
        "a {} refusal moves one family by one",
        reason.as_str()
    );
    assert_eq!(
        s.b2bua.new_calls().rejected(reason, class),
        refused_before + 1,
        "counted under {} and its own class",
        reason.as_str()
    );
}

async fn finish(s: B2buaScene) {
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}

/// The rungs in the router: capacity, the queue cap, panic-ELU, the bucket.
#[tokio::test(start_paused = true)]
async fn a_router_rung_refusal_moves_one_family_once() {
    let (probe, _system) = simulated();
    let (sampler, load) = load_shed::simulated();
    let s = B2buaScene::with_b2bua("one-count-router-rungs", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port)
            .capacity(CapacityGate::new(Arc::new(probe)))
            .overload(OverloadSignal::new(Arc::new(sampler)))
            .tune(|c| {
                c.cps_bucket_size = 1;
                c.cps_bucket_rate = 0;
                c.overload_panic_elu_threshold = 0.5;
                c.per_call_queue_cap = 1;
            })
    })
    .await;

    // One live call holds the only token and the only per-call queue.
    let mut first = s.establish().await;
    let gate = s.b2bua.capacity().clone();
    gate.configure(&CapacityConfig {
        calls: Ceilings { normal: Some(1), emergency: Some(1) },
        ..Default::default()
    });
    refused_once(&s, &s.alice, true, Refusal::CapacityCalls, true).await;
    gate.configure(&CapacityConfig::default());
    refused_once(&s, &s.alice, false, Refusal::CapShed, true).await;
    s.hangup(&mut first).await;
    let m = s.b2bua.metrics();
    settle_until(|| s.b2bua.active_calls() == 0 && m.removals_total() == m.creations_total()).await;

    let overload = s.b2bua.overload().clone();
    load.set_elu(0.9);
    settle_until(|| overload.metrics().elu_ewma > 0.5).await;
    refused_once(&s, &s.alice, false, Refusal::PanicElu, true).await;
    load.set_elu(0.0);
    settle_until(|| overload.metrics().elu_ewma < 0.5).await;
    refused_once(&s, &s.alice, false, Refusal::BucketEmpty, true).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    finish(s).await;
}

/// The ingress brake, engaged from the first datagram; an emergency INVITE
/// passes it and is admitted.
#[tokio::test(start_paused = true)]
async fn a_brake_refusal_moves_one_family_once() {
    let s = B2buaScene::with_b2bua("one-count-brake", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port)
            .ingress_brake(IngressBrakeConfig { queue_max: 256, threshold_pct: 0 })
    })
    .await;

    refused_once(&s, &s.alice, false, Refusal::IngressBrake, true).await;
    let mut urgent = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header(EMERGENCY.0, EMERGENCY.1)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(callflow::ANSWER_SDP).await;
    urgent.expect(200).await;
    let mut dialog = urgent.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    finish(s).await;
}

/// The transaction layer's backlog at a ceiling of 0 for every class: a
/// normal and an emergency INVITE are each counted once, in their own class.
#[tokio::test]
async fn a_backlog_refusal_moves_one_family_once_in_its_class() {
    let s = B2buaScene::with_b2bua("one-count-backlog", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).deferred_backlog_ceilings(0, 0)
    })
    .await;

    refused_once(&s, &s.alice, false, Refusal::DeferredBacklog, true).await;
    refused_once(&s, &s.alice, true, Refusal::DeferredBacklog, true).await;
    finish(s).await;
}
