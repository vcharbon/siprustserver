//! Every new INVITE is counted once on `b2bua_new_calls_total`, by the outcome
//! its admission tier reached: accepted, or rejected with the tier's reason.
//! Each INVITE here is retransmitted before its final arrives (RFC 3261
//! §17.1.1.2), and the copy is never counted again. The counts add up to the
//! INVITEs sent, and every call is set up and torn down properly.
//!
//! The deferred-backlog tier refuses in the transaction layer, behind a full
//! event queue; its once-per-INVITE count is pinned in `sip-txn`'s
//! `bounded_queue` tests and its share of the composed count in
//! `b2bua::new_calls`.

use std::sync::Arc;

use b2bua::capacity::{simulated, CapacityGate, Level};
use b2bua::config::{CapacityConfig, Ceilings};
use b2bua::new_calls::{NewCallCounts, Refusal};
use b2bua::overload::OverloadSignal;
use b2bua::store::{StoreFaultPoint, StoreFaults};
use b2bua::tier1_brake::Tier1BrakeConfig;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, OFFER_SDP};
use scenario_harness::{callflow, Dialog};

const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const EMERGENCY: (&str, &str) = ("Resource-Priority", "esnet.0");

/// A confirmed call whose INVITE was sent twice: the copy reaches the INVITE
/// server transaction, which absorbs it.
async fn establish_retransmitted(s: &B2buaScene, emergency: bool) -> Dialog {
    let mut invite = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr);
    if emergency {
        invite = invite.with_header(EMERGENCY.0, EMERGENCY.1);
    }
    let mut call = invite.send().await;
    call.retransmit().await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    dialog
}

/// A new INVITE, sent twice, refused with `status`; bob is never reached.
async fn refused_retransmitted(s: &B2buaScene, emergency: bool, status: u16) {
    let mut invite = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr);
    if emergency {
        invite = invite.with_header(EMERGENCY.0, EMERGENCY.1);
    }
    let mut call = invite.send().await;
    call.retransmit().await;
    call.expect(status).await;
}

/// Wait until every per-call queue this worker opened is released, so the
/// next INVITE finds the global queue cap free.
async fn settled(s: &B2buaScene) {
    let m = s.b2bua.metrics();
    settle_until(|| s.b2bua.active_calls() == 0 && m.removals_total() == m.creations_total()).await;
    assert_eq!(m.removals_total(), m.creations_total(), "a per-call queue is still open");
}

/// Every series of `counts` other than the ones listed is 0.
fn assert_counts(counts: &NewCallCounts, accepted: [u64; 2], rejected: &[(Refusal, bool, u64)]) {
    assert_eq!(counts.accepted(false), accepted[0], "accepted normal");
    assert_eq!(counts.accepted(true), accepted[1], "accepted emergency");
    for reason in Refusal::ALL {
        for emergency in [false, true] {
            let want = rejected
                .iter()
                .find(|(r, e, _)| *r == reason && *e == emergency)
                .map_or(0, |(_, _, n)| *n);
            assert_eq!(
                counts.rejected(reason, emergency),
                want,
                "rejected {} emergency={emergency}",
                reason.as_str()
            );
        }
    }
}

/// One worker, one INVITE down each path the router decides on: accepted
/// (normal and emergency), each memory bound, the global queue cap, the
/// store-fault 500, the panic-ELU backstop and the empty CPS bucket.
///
/// The bucket holds three tokens and never refills. The two accepts and the
/// panic-ELU reject (judged after its token) take one each; the memory
/// bounds, the queue cap and the store fault are judged before the bucket and
/// take none. The last INVITE finds the bucket empty.
#[tokio::test]
async fn every_new_invite_is_counted_once_by_its_outcome() {
    let faults = StoreFaults::default();
    let (probe, system) = simulated();
    let (sampler, load) = b2bua::overload::simulated();
    let s = B2buaScene::with_b2bua("b2bua-new-call-outcomes", {
        let faults = faults.clone();
        move |bob_port| {
            B2buaSut::route_all_to("127.0.0.1", bob_port)
                .with_store_faults(faults)
                .capacity(CapacityGate::new(Arc::new(probe)))
                .overload(OverloadSignal::new(Arc::new(sampler)))
                .tune(|c| {
                    c.cps_bucket_size = 3;
                    c.cps_bucket_rate = 0;
                    c.overload_panic_elu_threshold = 0.5;
                    c.per_call_queue_cap = 2;
                })
        }
    })
    .await;
    let gate = s.b2bua.capacity().clone();
    let overload = s.b2bua.overload().clone();
    assert_eq!(s.b2bua.new_calls().total(), 0);

    // One live call: the call ceiling, then the transaction ceiling, refuse.
    let mut first = establish_retransmitted(&s, false).await;
    gate.configure(&CapacityConfig {
        calls: Ceilings { normal: Some(1), emergency: None },
        ..Default::default()
    });
    refused_retransmitted(&s, false, 503).await;
    gate.configure(&CapacityConfig {
        transactions: Ceilings { normal: Some(1), emergency: Some(1) },
        ..Default::default()
    });
    assert!(s.b2bua.txn_metrics().active_transactions() >= 1, "the live call holds a transaction");
    refused_retransmitted(&s, true, 503).await;
    gate.configure(&CapacityConfig::default());

    // A second live call holds the last per-call queue: the next new call is
    // shed at the cap.
    let mut urgent = establish_retransmitted(&s, true).await;
    refused_retransmitted(&s, false, 503).await;
    s.hangup(&mut first).await;
    callflow::hangup(&mut urgent, &s.bob).await;
    settled(&s).await;

    // The call store fails the initial-INVITE probe: 500.
    faults.arm(StoreFaultPoint::LiveInitialInvite);
    refused_retransmitted(&s, false, 500).await;
    faults.disarm(StoreFaultPoint::LiveInitialInvite);
    settled(&s).await;

    // RSS between the two ceilings refuses a normal call.
    gate.configure(&CapacityConfig {
        rss_bytes: Ceilings { normal: Some(1_000), emergency: Some(2_000) },
        ..Default::default()
    });
    system.set_rss_bytes(Some(1_500));
    settle_until(|| gate.level() == Level::ShedNormal).await;
    refused_retransmitted(&s, false, 503).await;
    settled(&s).await;
    gate.configure(&CapacityConfig::default());
    system.set_rss_bytes(Some(10));
    settle_until(|| gate.level() == Level::Open).await;

    // The worker's own loop above the panic backstop.
    load.set_elu(0.9);
    settle_until(|| overload.metrics().elu_ewma > 0.5).await;
    refused_retransmitted(&s, false, 503).await;
    load.set_elu(0.0);
    settle_until(|| overload.metrics().elu_ewma < 0.5).await;
    settled(&s).await;

    // The bucket is empty.
    refused_retransmitted(&s, false, 503).await;
    settled(&s).await;

    let counts = s.b2bua.new_calls();
    assert_counts(
        &counts,
        [1, 1],
        &[
            (Refusal::CapacityCalls, false, 1),
            (Refusal::CapacityTransactions, true, 1),
            (Refusal::CapShed, false, 1),
            (Refusal::StoreFault, false, 1),
            (Refusal::CapacityRss, false, 1),
            (Refusal::PanicElu, false, 1),
            (Refusal::BucketEmpty, false, 1),
        ],
    );
    assert_eq!(counts.total(), 9, "one count per INVITE sent");
    settle_until(|| s.b2bua.cdr_records().len() == 2).await;
    assert_eq!(s.b2bua.cdr_records().len(), 2, "one CDR per accepted call");
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}

/// The ingress brake sheds a new non-emergency INVITE statelessly, before any
/// transaction exists, and answers its copy the same: two 503s on the wire,
/// one rejected new call. An emergency INVITE passes it and is accepted.
#[tokio::test]
async fn the_ingress_brake_counts_a_shed_invite_once() {
    let s = B2buaScene::with_b2bua("b2bua-new-call-brake", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tier1_brake(Tier1BrakeConfig {
            queue_max: 256,
            tier1_threshold_pct: 0,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 0,
        })
    })
    .await;

    let mut urgent = establish_retransmitted(&s, true).await;
    callflow::hangup(&mut urgent, &s.bob).await;
    settled(&s).await;

    refused_retransmitted(&s, false, 503).await;
    let brake = s.b2bua.tier1_brake().expect("the brake is installed");
    settle_until(|| brake.drops_tier1_brake() == 2).await;
    assert_eq!(brake.drops_tier1_brake(), 2, "both copies are answered");

    let counts = s.b2bua.new_calls();
    assert_counts(&counts, [0, 1], &[(Refusal::Tier1Brake, false, 1)]);
    assert_eq!(counts.total(), 2, "one count per INVITE sent");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}

/// The brake at a real threshold, an INVITE's copies on each side of it: shed
/// above, its copy below draws the same 503 and is never admitted behind the
/// caller's back. One INVITE, one count; a later INVITE below the threshold is
/// accepted.
#[tokio::test]
async fn a_shed_invite_copy_below_the_brake_threshold_is_shed_again() {
    let s = B2buaScene::with_b2bua("b2bua-new-call-brake-threshold", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tier1_brake(Tier1BrakeConfig {
            queue_max: 256,
            tier1_threshold_pct: 50,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 0,
        })
    })
    .await;
    let brake = s.b2bua.tier1_brake().expect("the brake is installed").clone();

    s.b2bua.force_brake_depth(Some(200));
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    settle_until(|| brake.drops_tier1_brake() == 1).await;
    s.b2bua.force_brake_depth(Some(0));
    call.retransmit().await;
    settle_until(|| brake.drops_tier1_brake() == 2).await;
    call.expect(503).await;
    assert_eq!(
        brake.drops_tier1_brake(),
        2,
        "the copy below the threshold is answered by the brake"
    );
    assert_eq!(s.b2bua.new_calls().accepted(false), 0, "the shed call is never admitted");

    s.b2bua.force_brake_depth(None);
    let mut next = establish_retransmitted(&s, false).await;
    s.hangup(&mut next).await;
    settled(&s).await;

    let counts = s.b2bua.new_calls();
    assert_counts(&counts, [1, 0], &[(Refusal::Tier1Brake, false, 1)]);
    assert_eq!(counts.total(), 2, "one count per INVITE sent");
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    s.b2bua.assert_fully_reaped();
    s.finish().await;
}
