//! An operator cordon (ADR-0050 §9): a cordoned worker answers an
//! out-of-dialog OPTIONS `200` refusing new calls (`cause=cordoned`), refuses a
//! new INVITE with the bare `503` and the record of a call no decision
//! reached — the record an unavailable decision leaves —, serves the dialog it
//! already holds, and accepts again once the cordon is lifted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::cdr::CdrRecord;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use b2bua::repl::ReadinessState;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::{CdrEventType, TerminationCause};
use sip_message::generators::OutOfDialogMethod;
use sip_message::worker_report::{self, Readiness};
use sip_message::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Routes every new call to bob, or fails as an unreachable decision backend
/// does while `down` holds.
struct Decision {
    bob_port: u16,
    down: Arc<AtomicBool>,
}

#[async_trait]
impl CallDecisionEngine for Decision {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        match self.down.load(Ordering::SeqCst) {
            true => Err(CallDecisionError::Unavailable("backend down".into())),
            false => Ok(NewCallResponse::Route(route_to("127.0.0.1", self.bob_port))),
        }
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no failure treatment".into()))
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no transfer".into()))
    }
}

/// The SUT's answer to alice's out-of-dialog OPTIONS: status and acceptance.
async fn options(s: &B2buaScene) -> (u16, Option<Readiness>) {
    let mut options =
        s.alice.request(OutOfDialogMethod::Options, &s.bob).through(s.b2bua.addr).send().await;
    let resp = options.expect(200).await;
    (resp.status(), worker_report::readiness(&resp))
}

/// A new call alice places, answered `503` by the SUT itself.
async fn refused(s: &B2buaScene) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let fin = call.expect(503).await;
    assert_eq!(fin.reason(), "Service Unavailable");
    for name in ["Retry-After", "Reason"] {
        assert_eq!(fin.raw(HeaderName::from(name)).count(), 0, "a bare 503: no {name}");
    }
}

/// What a record says of the call, its stamps aside.
type Shape =
    (Vec<(CdrEventType, String, Option<i64>, Option<String>, u32)>, usize, usize, TerminationCause);

fn shape(record: &CdrRecord) -> Shape {
    let events = record
        .events
        .iter()
        .map(|e| {
            (e.event_type, e.leg_id.clone(), e.status_code, e.reason.clone(), e.decision_ordinal)
        })
        .collect();
    let cause = record.termination.as_ref().expect("a terminated call holds its record").cause;
    (events, record.decision_log.len(), record.b_legs.len(), cause)
}

#[tokio::test(start_paused = true)]
async fn a_cordoned_worker_refuses_new_calls_with_their_record_and_serves_its_dialogs() {
    let down = Arc::new(AtomicBool::new(false));
    let decision = down.clone();
    let s = B2buaScene::with_b2bua("cordon", move |bob_port| {
        B2buaSut::builder(Arc::new(Decision { bob_port, down: decision }))
    })
    .await;
    let readiness = s.b2bua.readiness();
    let mut held = s.establish().await;
    assert_eq!(options(&s).await, (200, Some(Readiness::accept())));

    // ── cordoned: refusing new calls, serving the held dialog ───────────────
    readiness.cordon().expect("a ready worker takes a cordon");
    assert_eq!(readiness.state(), ReadinessState::Cordoned);
    assert_eq!(options(&s).await, (200, Some(Readiness::refuse("cordoned"))));
    refused(&s).await;
    s.hangup(&mut held).await;

    // ── uncordoned: accepting again ─────────────────────────────────────────
    readiness.uncordon().expect("a cordoned worker is uncordoned");
    assert_eq!(options(&s).await, (200, Some(Readiness::accept())));
    let mut after = s.establish().await;
    s.hangup(&mut after).await;

    // ── the same refusal from an unavailable decision ───────────────────────
    down.store(true, Ordering::SeqCst);
    refused(&s).await;
    down.store(false, Ordering::SeqCst);

    settle_until(|| s.b2bua.is_reaped()).await;
    let records = s.b2bua.cdr_records();
    assert_eq!(records.len(), 4, "one record per call, the refused ones included");
    let refusals: Vec<Shape> = records.iter().filter(|r| r.b_legs.is_empty()).map(shape).collect();
    let [by_cordon, by_decision] = refusals.as_slice() else {
        panic!("two calls refused before any callee: {refusals:#?}");
    };
    let last = by_cordon.0.last().map(|e| (e.0, e.2));
    assert_eq!(last, Some((CdrEventType::Reject, Some(503))));
    assert_eq!((by_cordon.1, by_cordon.3), (0, TerminationCause::Admission), "no decision");
    assert_eq!(by_cordon, by_decision, "the record of a call no decision reached");
    let _report = s.finish().await;
}
