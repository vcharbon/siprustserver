//! An initial INVITE whose turn waits for a new-call permit, CANCELed by its
//! caller meanwhile (RFC 3261 §9.1): the transaction layer answers 200 to the
//! CANCEL and 487 to the INVITE at once, and the call's turn, once it gets its
//! permit, ends the setup as cancelled. It asks the decision backend nothing,
//! admits no limiter hold, and is counted as cancelled, not accepted nor on
//! X-Overload `adm`; the call still writes the CDR of a setup cancelled before
//! routing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::admission::Class;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::{CdrEventType, TerminationCause};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::ClientInvite;
use tokio::sync::watch;

const CONCURRENCY: usize = 4;
/// Half the pool: two new-call permits.
const SHARE_PERCENT: u8 = 50;
/// The callers whose decisions hold the new-call share.
const HOLDERS: usize = 2;

/// Stalls every decision until the backend is released, then routes; records
/// the From of each decision asked.
struct Stalled {
    inner: ScriptedDecisionEngine,
    released: watch::Receiver<bool>,
    asked: Mutex<Vec<String>>,
    count: AtomicUsize,
}

#[async_trait]
impl CallDecisionEngine for Stalled {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.asked.lock().unwrap().push(req.from.clone());
        self.count.fetch_add(1, Ordering::SeqCst);
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

/// The caller CANCELs its pending INVITE: 200 to the CANCEL, 487 to the
/// INVITE, both from the transaction layer.
async fn cancel(invite: &mut ClientInvite) {
    let mut cxl = invite.cancel().await;
    cxl.expect(200).await;
    invite.expect(487).await;
}

#[tokio::test(start_paused = true)]
async fn an_invite_canceled_while_waiting_for_a_permit_skips_the_decision_and_the_limiter() {
    let (release, released) = watch::channel(false);
    let mut engine = None;
    let s = B2buaScene::with_b2bua("b2bua-cancel-while-waiting-for-a-permit", |bob_port| {
        let e = Arc::new(Stalled {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            released,
            asked: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
        });
        engine = Some(e.clone());
        B2buaSut::builder(e).tune(|c| {
            c.event_dispatch_concurrency = CONCURRENCY;
            c.new_call_permit_share_percent = SHARE_PERCENT;
            // No decision deadline frees a permit while the scenario runs.
            c.call_control_timeout_ms = 60_000;
        })
    })
    .await;
    let engine = engine.expect("the engine was built");

    // Two calls take the new-call share and park on the stalled backend.
    let mut holders = Vec::new();
    for i in 0..HOLDERS {
        let caller = s.h.agent(&format!("carol{i}"), &format!("127.0.0.1:{}", 5062 + i)).await;
        holders.push(caller.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await);
    }
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(engine.count.load(Ordering::SeqCst), HOLDERS, "the share is held");

    // A third INVITE's turn waits for a new-call permit.
    let erin = s.h.agent("erin", "127.0.0.1:5069").await;
    let mut waiting = erin.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(300)).await;
    assert_eq!(
        s.b2bua.metrics().new_call_share_waits_total(),
        1,
        "the third INVITE's turn waits for a permit of the new-call share"
    );

    // Its caller gives up; then every holder's caller does, and the backend
    // answers.
    cancel(&mut waiting).await;
    for invite in &mut holders {
        cancel(invite).await;
    }
    release.send_replace(true);
    settle_until(|| s.b2bua.cdr_records().len() == HOLDERS + 1 && s.b2bua.is_reaped()).await;

    let asked = engine.asked.lock().unwrap().clone();
    assert!(
        !asked.iter().any(|from| from.contains("erin")),
        "the cancelled setup asked the decision backend nothing: {asked:?}"
    );
    assert_eq!(asked.len(), HOLDERS, "one decision per holder: {asked:?}");
    assert_eq!(
        s.b2bua.limiter_count().admitted,
        HOLDERS as i64,
        "only the holders' routes admitted a limiter hold"
    );
    assert_eq!(
        s.b2bua.metrics().decision_dropped_cancelled_total(),
        HOLDERS as u64,
        "each holder's decision landed on its cancelled call and was dropped"
    );

    let cdrs = s.b2bua.cdr_records();
    let cdr = cdrs
        .iter()
        .find(|r| r.a_leg.call_id == waiting.call_id())
        .expect("the cancelled setup wrote its CDR");
    assert!(cdr.b_legs.is_empty(), "no b-leg: {cdr:?}");
    assert!(cdr.decision_log.is_empty(), "no decision applied: {cdr:?}");
    let events: Vec<_> = cdr.events.iter().map(|e| e.event_type).collect();
    assert_eq!(events, [CdrEventType::InviteReceived, CdrEventType::Cancel], "{cdr:?}");
    assert_eq!(
        cdr.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteCancel),
        "{cdr:?}"
    );

    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), HOLDERS as u64, "the holders were accepted");
    assert_eq!(counts.cancelled(Class::Normal), 1, "the waiting INVITE counts as cancelled");
    assert_eq!(counts.total(), HOLDERS as u64 + 1, "one count per INVITE sent");
    let header = s.b2bua.overload().x_overload_header_value();
    assert!(
        header.ends_with(&format!("adm={HOLDERS}")),
        "the cancelled setup is not counted as treated: {header}"
    );

    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no cancelled call reaches bob"
    );
    let _ = s.finish().await;
}
