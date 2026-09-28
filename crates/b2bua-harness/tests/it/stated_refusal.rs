//! An adapter's stated refusal: a decision engine that read no decision
//! refuses the call with a final it states (`CallDecisionError::Refused`).
//! The stack answers it exactly as a decision reject — the same code, the
//! same reason policy, the same stated headers — on the initial INVITE and
//! on both failover consults, yet marks no decision: the final is the
//! stack's own (`TerminationCause::Admission`).

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::config::CdrConfig;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallTreatment, HeaderUpdate,
    NewCallRequest, NewCallResponse, RejectDecision, SipHeaderUpdates,
};
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use b2bua_harness::{settle_until, B2buaSut};
use call::{Call, DecisionKind, TerminationCause};
use scenario_harness::Harness;
use sip_message::header::HeaderName;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const CAROL: &str = "127.0.0.1:5070";
const B2BUA: &str = "127.0.0.1:5080";

/// What the scripted engine answers at one decision point.
#[derive(Clone, Copy)]
enum Answer {
    /// A decision reject, labelled `deny`.
    Reject,
    /// The adapter's stated refusal.
    Refused,
}

fn stated_headers() -> Option<SipHeaderUpdates> {
    Some([("X-Refusal".to_string(), HeaderUpdate::line("stated"))].into_iter().collect())
}

fn reject(code: u16, reason: Option<&str>) -> RejectDecision {
    RejectDecision {
        reject_code: code,
        reject_reason: reason.map(str::to_string),
        update_headers: stated_headers(),
        service_ext: Default::default(),
        label: Some("deny".into()),
    }
}

fn refused(code: u16, reason: Option<&str>) -> CallDecisionError {
    CallDecisionError::Refused {
        code,
        reason: reason.map(str::to_string),
        update_headers: stated_headers(),
    }
}

/// `new_call` routes to Carol (with a token, so a failover is consulted, and
/// behind the `cap` limiter when `capped`) or answers `initial`; every
/// `call_failure` answers `failure`; a reject or refusal states `reason`.
struct Engine {
    initial: Option<Answer>,
    capped: bool,
    failure: Answer,
    reason: Option<&'static str>,
}

#[async_trait]
impl CallDecisionEngine for Engine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        match self.initial {
            Some(Answer::Reject) => Ok(NewCallResponse::Reject(reject(500, self.reason))),
            Some(Answer::Refused) => Err(refused(500, self.reason)),
            None => {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.label = Some("first".into());
                if self.capped {
                    r.call_limiter = vec![CallLimiterEntry { id: "cap".into(), limit: 1 }];
                }
                Ok(NewCallResponse::Route(r))
            }
        }
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        match self.failure {
            Answer::Reject => Ok(CallTreatment::Reject(reject(503, self.reason))),
            Answer::Refused => Err(refused(503, self.reason)),
        }
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Ok(CallReferResponse::Reject { code: 501, reason: None, label: None })
    }
}

/// A limiter that refuses the `cap` admission.
struct RefusingLimiter;

#[async_trait]
impl CallLimiter for RefusingLimiter {
    async fn admit(&self, _: &str, entries: &[LimiterEntry], _: bool) -> AdmitOutcome {
        match entries.iter().find(|e| e.id == "cap") {
            Some(e) => AdmitOutcome::Rejected { limiter_id: e.id.clone() },
            None => AdmitOutcome::Unavailable,
        }
    }
    async fn release(&self, _keys: &[String]) -> ReleaseAnswer {
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: b2bua::limiter::LimiterReports) {}
}

/// The SUT under `engine`, keeping every terminated `Call` whole; a capped
/// engine runs behind the refusing limiter.
struct Sut {
    addr: SocketAddr,
    b2bua: B2buaSut,
}

impl Sut {
    async fn spawn(h: &Harness, engine: Engine) -> Self {
        let capped = engine.capped;
        let mut builder = B2buaSut::builder(Arc::new(engine)).keep_terminated_calls().tune(|c| {
            c.worker_allowed_target_suffixes = vec!["*".into()];
            c.cdr = CdrConfig { message_ring: 32, captured_headers: Vec::new() };
        });
        if capped {
            builder = builder.limiter(Arc::new(RefusingLimiter));
        }
        let b2bua = builder.start(h, "b2bua", B2BUA).await;
        Self { addr: b2bua.addr, b2bua }
    }

    /// Every call created is reaped, every limiter hold released, and the
    /// one CDR is written.
    async fn assert_reaped(&self) -> Call {
        settle_until(|| self.b2bua.is_reaped() && self.b2bua.cdr_records().len() == 1).await;
        self.b2bua.assert_fully_reaped();
        let terminated = self.b2bua.terminated_calls();
        assert_eq!(terminated.len(), 1, "exactly one CDR per call");
        terminated.into_iter().next().unwrap()
    }
}

/// The caller's final as the wire shows it: code, phrase, the stated header.
fn final_of(resp: &SipResponse) -> (u16, String, Vec<String>) {
    let stated = resp.raw(HeaderName::from("X-Refusal")).map(str::to_string).collect();
    (resp.status(), resp.reason().to_string(), stated)
}

fn marks(call: &Call) -> Vec<(DecisionKind, Option<&str>, Option<&str>)> {
    call.decision_log.iter().map(|m| (m.kind, m.leg_id.as_deref(), m.label.as_deref())).collect()
}

fn cause(call: &Call) -> TerminationCause {
    call.termination.as_ref().expect("a terminated call holds its record").cause
}

/// The decision ordinal the caller's `code` final was sent under.
fn final_ordinal(call: &Call, code: u16) -> u32 {
    call.a_leg
        .messages
        .entries
        .iter()
        .rfind(|e| e.method == "INVITE" && e.code == Some(code))
        .expect("the caller's final in the ring")
        .decision_ordinal
}

/// The initial INVITE refused: nothing but the caller's INVITE and its final.
async fn initial(
    name: &str,
    answer: Answer,
    reason: Option<&'static str>,
) -> ((u16, String, Vec<String>), Call) {
    let h = Harness::new(name);
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let engine = Engine { initial: Some(answer), capped: false, failure: Answer::Reject, reason };
    let sut = Sut::spawn(&h, engine).await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(sut.addr).send().await;
    let seen = final_of(&call.expect(500).await);
    let done = sut.assert_reaped().await;
    let _report = h.finish().await;
    (seen, done)
}

#[tokio::test(start_paused = true)]
async fn a_refused_initial_invite_is_answered_as_a_reject_and_marks_nothing() {
    let (decided, by_decision) = initial("refusal-initial-decided", Answer::Reject, None).await;
    let (refused, by_stack) = initial("refusal-initial-refused", Answer::Refused, None).await;

    assert_eq!(
        decided,
        (500, "Server Internal Error".into(), vec!["stated".into()]),
        "the decision reject's final"
    );
    assert_eq!(refused, decided, "the refusal is answered as the decision reject");

    assert_eq!(marks(&by_decision), vec![(DecisionKind::Reject, Some("a"), Some("deny"))]);
    assert!(by_stack.decision_log.is_empty(), "no decision: {:?}", by_stack.decision_log);
    assert_eq!(by_stack.decision_ordinal, 0);
    assert_eq!(final_ordinal(&by_decision, 500), 1);
    assert_eq!(final_ordinal(&by_stack, 500), 0, "the final precedes any decision");
    assert_eq!(cause(&by_decision), TerminationCause::DecisionReject);
    assert_eq!(cause(&by_stack), TerminationCause::Admission, "the stack's own refusal");
}

/// A routed call whose callee rings, then answers 486, and whose failure
/// consult answers `failure`: the final goes out on the early dialog the
/// relayed 180 opened.
async fn failover(
    name: &str,
    failure: Answer,
    reason: Option<&'static str>,
) -> ((u16, String, Vec<String>), Call) {
    let h = Harness::new(name);
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let sut = Sut::spawn(&h, Engine { initial: None, capped: false, failure, reason }).await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    carol.receive("ACK").await;
    let fin = call.expect(503).await;
    assert_eq!(
        fin.to().tag(),
        ringing.to().tag(),
        "the final keeps the To-tag of the early dialog the 180 opened"
    );
    let seen = final_of(&fin);
    let done = sut.assert_reaped().await;
    let _report = h.finish().await;
    (seen, done)
}

#[tokio::test(start_paused = true)]
async fn a_refused_failure_consult_is_answered_as_a_reject_and_marks_nothing() {
    let (decided, by_decision) = failover("refusal-failover-decided", Answer::Reject, None).await;
    let (refused, by_stack) = failover("refusal-failover-refused", Answer::Refused, None).await;

    assert_eq!(
        decided,
        (503, "Service Unavailable".into(), vec!["stated".into()]),
        "the decision reject's final wears the code's phrase"
    );
    assert_eq!(refused, decided, "the refusal is answered as the decision reject");

    assert_eq!(
        marks(&by_decision),
        vec![
            (DecisionKind::Route, Some("a"), Some("first")),
            (DecisionKind::FailoverReject, Some("b-1"), Some("deny")),
        ]
    );
    assert_eq!(
        marks(&by_stack),
        vec![(DecisionKind::Route, Some("a"), Some("first"))],
        "the route stays the last decision"
    );
    assert_eq!(final_ordinal(&by_decision, 503), 2);
    assert_eq!(final_ordinal(&by_stack, 503), 1, "the final is sent under the route");
    assert_eq!(cause(&by_decision), TerminationCause::DecisionReject);
    assert_eq!(cause(&by_stack), TerminationCause::Admission, "the stack's own refusal");
}

/// A route the limiter refuses, whose failover consult answers `failure`.
async fn limiter(name: &str, failure: Answer) -> ((u16, String, Vec<String>), Call) {
    let h = Harness::new(name);
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let sut = Sut::spawn(&h, Engine { initial: None, capped: true, failure, reason: None }).await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(sut.addr).send().await;
    let seen = final_of(&call.expect(503).await);
    assert_eq!(carol.drain().await, 0, "the refused route dialed nothing");
    let done = sut.assert_reaped().await;
    let _report = h.finish().await;
    (seen, done)
}

#[tokio::test(start_paused = true)]
async fn a_refused_limiter_failover_is_answered_as_a_reject_and_marks_nothing() {
    let (decided, by_decision) = limiter("refusal-limiter-decided", Answer::Reject).await;
    let (refused, by_stack) = limiter("refusal-limiter-refused", Answer::Refused).await;

    assert_eq!(decided, (503, "Service Unavailable".into(), vec!["stated".into()]));
    assert_eq!(refused, decided, "the refusal is answered as the decision reject");

    assert_eq!(marks(&by_decision), vec![(DecisionKind::FailoverReject, None, Some("deny"))]);
    assert!(by_stack.decision_log.is_empty(), "no decision: {:?}", by_stack.decision_log);
    assert_eq!(final_ordinal(&by_decision, 503), 1);
    assert_eq!(final_ordinal(&by_stack, 503), 0);
    assert_eq!(cause(&by_decision), TerminationCause::DecisionReject);
    assert_eq!(cause(&by_stack), TerminationCause::Admission, "the stack's own refusal");
}

/// A refusal that states its reason wears it, on the initial path and on the
/// async failover, exactly as a decision reject stating the same reason.
#[tokio::test(start_paused = true)]
async fn a_refusal_stating_its_reason_wears_it() {
    const STATED: Option<&str> = Some("Not Implemented Here");
    let (decided, _) = initial("refusal-reason-initial-decided", Answer::Reject, STATED).await;
    let (refused, _) = initial("refusal-reason-initial-refused", Answer::Refused, STATED).await;
    assert_eq!(refused, (500, "Not Implemented Here".into(), vec!["stated".into()]));
    assert_eq!(refused, decided);

    let (decided, _) = failover("refusal-reason-failover-decided", Answer::Reject, STATED).await;
    let (refused, _) = failover("refusal-reason-failover-refused", Answer::Refused, STATED).await;
    assert_eq!(refused, (503, "Not Implemented Here".into(), vec!["stated".into()]));
    assert_eq!(refused, decided);
}
