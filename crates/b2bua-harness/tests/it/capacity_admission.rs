//! Capacity admission (ADR-0037) through a running `B2buaCore`: at a live-call,
//! transaction or RSS ceiling a new INVITE draws a 503 carrying `Retry-After`
//! and no `Reason`, from the INVITE server transaction; emergency calls keep
//! priority up to their own ceiling; no call state is born for a reject; and
//! admission reopens once the worker is back under the ceiling. The gate's
//! unit behaviour is pinned in `b2bua::capacity::tests`; this file proves the
//! wiring, with every call properly set up and torn down.
//!
//! The RSS cases drive the reading through an injected simulated probe and
//! wait for the core's 100 ms sampler to take it.

use std::sync::Arc;

use b2bua::capacity::{simulated, Bound, CapacityGate, Level, SimulatedSystemControl};
use b2bua::config::{CapacityConfig, Ceilings};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow;
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::header::{Reason, RetryAfter};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const EMERGENCY: (&str, &str) = ("Resource-Priority", "esnet.0");

/// A confirmed call alice → b2bua → bob; `emergency` marks the INVITE.
async fn establish(s: &B2buaScene, emergency: bool) -> Dialog {
    if !emergency {
        return s.establish().await;
    }
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header(EMERGENCY.0, EMERGENCY.1)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    dialog
}

/// A new INVITE the gate refuses: the 503 shape, and bob never reached.
async fn expect_refused(alice: &Agent, bob: &Agent, b2bua: &B2buaSut, emergency: bool) {
    let mut invite = alice.invite(bob).with_sdp(OFFER).through(b2bua.addr);
    if emergency {
        invite = invite.with_header(EMERGENCY.0, EMERGENCY.1);
    }
    let mut call = invite.send().await;
    let resp = call.expect(503).await;
    assert!(resp.header::<RetryAfter>().is_some(), "a capacity 503 carries Retry-After");
    assert!(resp.header::<Reason>().is_none(), "a capacity 503 carries no Reason");
    assert!(resp.to().tag().is_some(), "non-100 final carries a To-tag (RFC §8.2.6.2)");
}

/// Every call ended and every per-call resource released, the refused
/// INVITEs' orphan queues included.
async fn assert_all_released(b2bua: &B2buaSut, cdrs: usize) {
    settle_until(|| b2bua.cdr_records().len() == cdrs).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

fn simulated_gate() -> (CapacityGate, SimulatedSystemControl) {
    let (probe, control) = simulated();
    (CapacityGate::new(Arc::new(probe)), control)
}

/// At the normal call ceiling a non-emergency call is refused while an
/// emergency one is admitted; at the emergency ceiling every call is refused;
/// once calls end, admission reopens.
#[tokio::test]
async fn the_call_ceilings_refuse_then_reopen() {
    let s = B2buaScene::with_b2bua("b2bua-capacity-calls", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.capacity.calls = Ceilings { normal: Some(1), emergency: Some(2) };
        })
    })
    .await;

    let mut first = establish(&s, false).await;
    expect_refused(&s.alice, &s.bob, &s.b2bua, false).await;
    let mut urgent = establish(&s, true).await;
    expect_refused(&s.alice, &s.bob, &s.b2bua, true).await;

    let gate = s.b2bua.capacity();
    assert_eq!(gate.rejected_total(Bound::Calls, false), 1);
    assert_eq!(gate.rejected_total(Bound::Calls, true), 1);
    assert_eq!(s.b2bua.active_calls(), 2, "a refused INVITE creates no live call");

    s.hangup(&mut first).await;
    callflow::hangup(&mut urgent, &s.bob).await;
    settle_until(|| s.b2bua.active_calls() == 0).await;

    let mut again = establish(&s, false).await;
    s.hangup(&mut again).await;
    assert_all_released(&s.b2bua, 3).await;
    assert_eq!(s.b2bua.metrics().overload_rejected_total(), 0, "no CPS or ELU shed");
    s.finish().await;
}

/// While the first call's transactions are live, a transaction ceiling of 1
/// refuses the next new call; the refused INVITE's own server transaction is
/// not counted against it.
#[tokio::test]
async fn the_transaction_ceiling_refuses_a_new_call() {
    let s = B2buaScene::with_b2bua("b2bua-capacity-transactions", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.capacity.transactions = Ceilings { normal: Some(1), emergency: None };
        })
    })
    .await;

    let mut first = establish(&s, false).await;
    assert!(s.b2bua.txn_metrics().active_transactions() >= 1);
    expect_refused(&s.alice, &s.bob, &s.b2bua, false).await;
    assert_eq!(s.b2bua.capacity().rejected_total(Bound::Transactions, false), 1);

    s.hangup(&mut first).await;
    assert_all_released(&s.b2bua, 1).await;
    s.finish().await;
}

/// The RSS ceilings read the injected probe: between them a non-emergency call
/// is refused and an emergency one admitted, above the emergency one every
/// call is refused, and below both admission reopens.
#[tokio::test]
async fn the_rss_ceilings_follow_the_sampled_reading() {
    let (gate, system) = simulated_gate();
    let s = B2buaScene::with_b2bua("b2bua-capacity-rss", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).capacity(gate).tune(|c| {
            c.capacity.rss_bytes = Ceilings { normal: Some(1_000), emergency: Some(2_000) };
        })
    })
    .await;
    let gate = s.b2bua.capacity().clone();

    system.set_rss_bytes(Some(1_500));
    settle_until(|| gate.level() == Level::ShedNormal).await;
    expect_refused(&s.alice, &s.bob, &s.b2bua, false).await;
    let mut urgent = establish(&s, true).await;

    system.set_rss_bytes(Some(2_500));
    settle_until(|| gate.level() == Level::ShedAll).await;
    expect_refused(&s.alice, &s.bob, &s.b2bua, true).await;
    callflow::hangup(&mut urgent, &s.bob).await;

    system.set_rss_bytes(Some(10));
    settle_until(|| gate.level() == Level::Open).await;
    let mut normal = establish(&s, false).await;
    s.hangup(&mut normal).await;

    assert_eq!(gate.rejected_total(Bound::Rss, false), 1);
    assert_eq!(gate.rejected_total(Bound::Rss, true), 1);
    assert_all_released(&s.b2bua, 2).await;
    s.finish().await;
}

/// A capacity reject spends no CPS token: with a two-token bucket that never
/// refills and one call slot, A takes a token, B is refused by the call
/// ceiling, and C still finds the second token once A has ended.
#[tokio::test]
async fn a_capacity_reject_spends_no_cps_token() {
    let s = B2buaScene::with_b2bua("b2bua-capacity-before-cps", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.cps_bucket_size = 2;
            c.cps_bucket_rate = 0;
            c.capacity.calls = Ceilings { normal: Some(1), emergency: None };
        })
    })
    .await;

    let mut a = establish(&s, false).await;
    expect_refused(&s.alice, &s.bob, &s.b2bua, false).await;
    s.hangup(&mut a).await;
    settle_until(|| s.b2bua.active_calls() == 0).await;
    let mut c = establish(&s, false).await;
    s.hangup(&mut c).await;

    assert_eq!(s.b2bua.capacity().rejected_total(Bound::Calls, false), 1);
    assert_eq!(s.b2bua.metrics().overload_rejected_total(), 0, "the bucket never ran dry");
    assert_all_released(&s.b2bua, 2).await;
    s.finish().await;
}

/// Past the new-call transaction ceiling, admitted calls keep all they are
/// owed (ADR-0037 item 7): while their in-dialog traffic holds the table
/// above it, a new call is refused 503, and the admitted calls' INFOs,
/// re-INVITEs and BYEs still complete, each call ending with its CDR.
#[tokio::test]
async fn past_the_new_call_ceiling_only_new_calls_are_refused() {
    let s = B2buaScene::with_b2bua("b2bua-capacity-margin", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.keepalive_interval_sec = 300)
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut dialogs =
        vec![establish(&s, false).await, callflow::establish(&carol, &s.bob, s.b2bua.addr).await];

    let txns = || s.b2bua.txn_metrics().active_transactions() as u64;
    let ceiling = txns() + 4;
    s.b2bua.capacity().configure(&CapacityConfig {
        transactions: Ceilings { normal: Some(ceiling), emergency: None },
        ..Default::default()
    });

    // The admitted calls' INFOs take the table past the ceiling, and on.
    for round in 0..12 {
        let dialog = &mut dialogs[round % 2];
        let mut info = dialog.send_request(InDialogMethod::Info).send().await;
        s.bob.receive("INFO").await.respond(200, "OK").await;
        info.expect(200).await;
    }
    assert!(txns() > ceiling, "in-dialog traffic past the new-call ceiling");
    expect_refused(&s.alice, &s.bob, &s.b2bua, false).await;

    // A re-INVITE of an admitted call is relayed and answered.
    let mut reinvite = dialogs[1].request(InDialogMethod::Invite, Some(OFFER)).await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    reinvite.expect(200).await;
    dialogs[1].ack(None).await;
    s.bob.receive("ACK").await;

    for dialog in &mut dialogs {
        s.hangup(dialog).await;
    }
    assert_eq!(s.b2bua.capacity().rejected_total(Bound::Transactions, false), 1);
    assert_all_released(&s.b2bua, 2).await;
    s.finish().await;
}
