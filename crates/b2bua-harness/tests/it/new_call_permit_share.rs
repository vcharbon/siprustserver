//! New calls hold only their share of the handler permits. A normal initial
//! INVITE's turn holds its permit across the decision round trip, so a burst
//! of new calls against a stalled decision backend would otherwise take every
//! permit and leave an established call's BYE waiting behind them. With the
//! share at half the pool, the burst holds half, an emergency INVITE still
//! reaches the backend from the rest, and the established call's BYE is
//! relayed and answered at once.
//!
//! Every burst INVITE is then CANCELed by its caller (RFC 3261 §9.1); the
//! backend answers once released, each answer lands on a cancelled call and is
//! dropped, a burst INVITE still waiting for a permit asks the backend nothing,
//! and every call ends.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::ClientInvite;
use tokio::sync::watch;

const EMERGENCY: (&str, &str) = ("Resource-Priority", "esnet.0");
const CONCURRENCY: usize = 4;
const SHARE_PERCENT: u8 = 50;
/// Normal new INVITEs in the burst: twice the new-call share.
const BURST: usize = 4;
/// What a BYE of an established call takes on an idle node, with margin: a
/// few transits and two turns.
const NORMAL_TEARDOWN: Duration = Duration::from_secs(1);

/// Routes the first call; every later decision stalls until the backend is
/// released, then routes. Counts the decisions asked, normal and emergency.
struct StalledAfterFirst {
    inner: ScriptedDecisionEngine,
    released: watch::Receiver<bool>,
    normal: AtomicUsize,
    emergency: AtomicUsize,
}

#[async_trait]
impl CallDecisionEngine for StalledAfterFirst {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        let first = if req.sip_header(EMERGENCY.0).is_some() {
            self.emergency.fetch_add(1, Ordering::SeqCst);
            false
        } else {
            self.normal.fetch_add(1, Ordering::SeqCst) == 0
        };
        if !first {
            let mut released = self.released.clone();
            let _ = released.wait_for(|r| *r).await;
        }
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

/// The caller CANCELs its pending INVITE: 200 to the CANCEL, 487 to the
/// INVITE, both from the transaction layer whatever the call's turn is doing.
async fn cancel(invite: &mut ClientInvite) {
    let mut cxl = invite.cancel().await;
    cxl.expect(200).await;
    invite.expect(487).await;
}

#[tokio::test(start_paused = true)]
async fn a_new_call_burst_on_a_stalled_backend_leaves_an_established_calls_bye_its_permit() {
    let (release, released) = watch::channel(false);
    let mut engine = None;
    let s = B2buaScene::with_b2bua("b2bua-new-call-permit-share", |bob_port| {
        let e = Arc::new(StalledAfterFirst {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            released,
            normal: AtomicUsize::new(0),
            emergency: AtomicUsize::new(0),
        });
        engine = Some(e.clone());
        B2buaSut::builder(e).tune(|c| {
            c.event_dispatch_concurrency = CONCURRENCY;
            c.new_call_permit_share_percent = SHARE_PERCENT;
            // The stall outlasts the scenario: no decision deadline frees a
            // permit while the BYE waits.
            c.call_control_timeout_ms = 60_000;
        })
    })
    .await;
    let engine = engine.expect("the engine was built");
    let mut holding = s.establish().await;
    settle_until(|| s.b2bua.active_calls() == 1).await;

    let mut burst = Vec::new();
    for i in 0..BURST {
        let caller = s.h.agent(&format!("carol{i}"), &format!("127.0.0.1:{}", 5062 + i)).await;
        let invite = caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
        burst.push(invite);
    }
    let dave = s.h.agent("dave", "127.0.0.1:5069").await;
    let mut urgent = dave
        .invite(&s.bob)
        .with_sdp(OFFER_SDP)
        .with_header(EMERGENCY.0, EMERGENCY.1)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.h.advance(Duration::from_millis(300)).await;

    // The established call's BYE is relayed and answered in normal time.
    let sent = tokio::time::Instant::now();
    let mut bye = holding.bye().await;
    let mut b_bye = s
        .bob
        .try_receive("BYE")
        .await
        .expect("bob hears the BYE while the new-call burst holds its share of the permits");
    b_bye.respond(200, "OK").await;
    bye.expect(200).await;
    assert!(
        sent.elapsed() < NORMAL_TEARDOWN,
        "the BYE was answered in {:?}, not within {NORMAL_TEARDOWN:?}",
        sent.elapsed()
    );
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;

    // The burst holds the share and no more; the emergency INVITE reached
    // the backend past it.
    assert_eq!(
        engine.normal.load(Ordering::SeqCst),
        1 + 2,
        "the established call's decision, then the new-call share of the burst"
    );
    assert_eq!(engine.emergency.load(Ordering::SeqCst), 1, "the emergency INVITE's turn ran");

    for invite in &mut burst {
        cancel(invite).await;
    }
    cancel(&mut urgent).await;
    release.send_replace(true);
    settle_until(|| s.b2bua.cdr_records().len() == 1 + BURST + 1 && s.b2bua.is_reaped()).await;
    assert_eq!(
        s.b2bua.cdr_records().len(),
        1 + BURST + 1,
        "one CDR per call: the established one and each CANCELed new call"
    );
    assert_eq!(
        engine.normal.load(Ordering::SeqCst),
        1 + 2,
        "a burst INVITE CANCELed while it waited for a permit asked no decision"
    );
    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no cancelled call reaches bob"
    );
    let _ = s.finish().await;
}
