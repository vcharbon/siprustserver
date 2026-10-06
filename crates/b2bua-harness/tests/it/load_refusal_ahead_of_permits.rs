//! The load rungs refuse a new INVITE before it waits for a handler permit.
//! New normal calls hold at most their share of the permits across their
//! decision round trip; against a stalled decision backend that share stays
//! held. A new INVITE the CPS bucket, the panic-ELU backstop or a capacity
//! ceiling refuses is answered 503 at once all the same, not after the stall.
//!
//! The two stalled INVITEs are then CANCELed by their caller (RFC 3261 §9.1),
//! the backend is released, each answer lands on a cancelled call and is
//! dropped, and every call ends with its CDR.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::admission::Class;
use b2bua::capacity::{simulated, CapacityGate};
use b2bua::config::{CapacityConfig, Ceilings};
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::new_calls::Refusal;
use b2bua::overload::OverloadSignal;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::{Agent, ClientInvite};
use tokio::sync::watch;

const CONCURRENCY: usize = 4;
const SHARE_PERCENT: u8 = 50;
/// The new-call share of the permits, and the INVITEs that stall holding it.
const STALLED: usize = 2;
/// A refusal answered at once: a few transits, no turn waited for.
const AT_ONCE: Duration = Duration::from_millis(500);

/// Every decision stalls until the backend is released, then routes.
struct Stalled {
    inner: ScriptedDecisionEngine,
    released: watch::Receiver<bool>,
}

#[async_trait]
impl CallDecisionEngine for Stalled {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        let mut released = self.released.clone();
        let _ = released.wait_for(|r| *r).await;
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// A new INVITE from `caller`, answered 503 within [`AT_ONCE`].
async fn refused_at_once(s: &B2buaScene, caller: &Agent) {
    let sent = tokio::time::Instant::now();
    let mut invite = caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    invite.expect(503).await;
    assert!(sent.elapsed() < AT_ONCE, "refused after {:?}, not at once", sent.elapsed());
}

/// The caller CANCELs its pending INVITE: 200 to the CANCEL, 487 to the
/// INVITE, both from the transaction layer whatever the call's turn is doing.
async fn cancel(invite: &mut ClientInvite) {
    let mut cxl = invite.cancel().await;
    cxl.expect(200).await;
    invite.expect(487).await;
}

#[tokio::test(start_paused = true)]
async fn a_load_refusal_does_not_wait_behind_a_held_new_call_share() {
    let (release, released) = watch::channel(false);
    let (sampler, load) = load_shed::simulated();
    let (probe, _system) = simulated();
    let s = B2buaScene::with_b2bua("b2bua-load-refusal-ahead-of-permits", |bob_port| {
        let engine = Arc::new(Stalled {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            released,
        });
        B2buaSut::builder(engine)
            .overload(OverloadSignal::new(Arc::new(sampler)))
            .capacity(CapacityGate::new(Arc::new(probe)))
            .tune(|c| {
                c.event_dispatch_concurrency = CONCURRENCY;
                c.new_call_permit_share_percent = SHARE_PERCENT;
                c.call_control_timeout_ms = 60_000;
                // One token per stalled call, never refilled.
                c.cps_bucket_size = STALLED as u32;
                c.cps_bucket_rate = 0;
                c.overload_panic_elu_threshold = 0.5;
            })
    })
    .await;

    let mut stalled = Vec::new();
    for i in 0..STALLED {
        let caller = s.h.agent(&format!("carol{i}"), &format!("127.0.0.1:{}", 5062 + i)).await;
        stalled.push(caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await);
    }
    settle_until(|| s.b2bua.active_calls() == STALLED).await;

    // The bucket is empty.
    let dave = s.h.agent("dave", "127.0.0.1:5066").await;
    refused_at_once(&s, &dave).await;

    // The loop is above the panic backstop.
    let overload = s.b2bua.overload().clone();
    load.set_elu(0.9);
    settle_until(|| overload.metrics().elu_ewma > 0.5).await;
    let erin = s.h.agent("erin", "127.0.0.1:5067").await;
    refused_at_once(&s, &erin).await;

    // The stalled calls hold the live-call ceiling.
    s.b2bua.capacity().configure(&CapacityConfig {
        calls: Ceilings { normal: Some(STALLED as u64), emergency: None },
        ..Default::default()
    });
    let frank = s.h.agent("frank", "127.0.0.1:5068").await;
    refused_at_once(&s, &frank).await;

    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::BucketEmpty, Class::Normal), 1);
    assert_eq!(counts.rejected(Refusal::PanicElu, Class::Normal), 1);
    assert_eq!(counts.rejected(Refusal::CapacityCalls, Class::Normal), 1);

    s.b2bua.capacity().configure(&CapacityConfig::default());
    load.set_elu(0.0);
    for invite in &mut stalled {
        cancel(invite).await;
    }
    release.send_replace(true);
    settle_until(|| s.b2bua.active_calls() == 0).await;
    settle_until(|| s.b2bua.cdr_records().len() == STALLED).await;
    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no cancelled call reaches bob"
    );
    let _ = s.finish().await;
}

/// A new INVITE admitted at ingress counts against the live-call ceiling from
/// its admission, before its turn — waiting behind the held new-call share —
/// creates its call. With the ceiling one above the stalled calls, the next
/// INVITE is admitted and waits; the one after it is refused 503 at once.
#[tokio::test(start_paused = true)]
async fn a_queued_new_call_counts_against_the_call_ceiling() {
    let (release, released) = watch::channel(false);
    let s = B2buaScene::with_b2bua("b2bua-queued-new-call-ceiling", |bob_port| {
        let engine = Arc::new(Stalled {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            released,
        });
        B2buaSut::builder(engine).tune(|c| {
            c.event_dispatch_concurrency = CONCURRENCY;
            c.new_call_permit_share_percent = SHARE_PERCENT;
            c.call_control_timeout_ms = 60_000;
        })
    })
    .await;

    let mut pending = Vec::new();
    for i in 0..STALLED {
        let caller = s.h.agent(&format!("carol{i}"), &format!("127.0.0.1:{}", 5062 + i)).await;
        pending.push(caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await);
    }
    settle_until(|| s.b2bua.active_calls() == STALLED).await;
    s.b2bua.capacity().configure(&CapacityConfig {
        calls: Ceilings { normal: Some(STALLED as u64 + 1), emergency: None },
        ..Default::default()
    });

    // Admitted: its turn waits for a new-call permit, no call yet.
    let dave = s.h.agent("dave", "127.0.0.1:5066").await;
    pending.push(dave.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await);
    s.h.advance(Duration::from_millis(100)).await;
    assert_eq!(s.b2bua.active_calls(), STALLED, "the admitted INVITE's turn has not run");

    // Its admission holds the ceiling's last call.
    let erin = s.h.agent("erin", "127.0.0.1:5067").await;
    refused_at_once(&s, &erin).await;
    assert_eq!(s.b2bua.new_calls().rejected(Refusal::CapacityCalls, Class::Normal), 1);

    s.b2bua.capacity().configure(&CapacityConfig::default());
    for invite in &mut pending {
        cancel(invite).await;
    }
    release.send_replace(true);
    settle_until(|| s.b2bua.active_calls() == 0).await;
    settle_until(|| s.b2bua.cdr_records().len() == STALLED + 1).await;
    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no cancelled call reaches bob"
    );
    let _ = s.finish().await;
}
