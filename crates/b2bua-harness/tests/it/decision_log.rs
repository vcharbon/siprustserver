//! The decision log on the replicated `Call`: one mark per decision the
//! decision layer returned and this stack applied, in order, each carrying the
//! opaque label the decision came with — and the count of marks at the
//! instant a message or event was written, stamped on it, so a later decision
//! replacing what the call holds never changes which decision a message was
//! handled under. The SUT is spawned bare so a probe [`CdrWriter`] hands the
//! test the terminated `Call` with its rings and log.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::config::{B2buaConfig, CdrConfig};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::rules::ServiceDef;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use b2bua_harness::settle_until;
use call::{Call, CdrEventType, DecisionKind, DecisionMark, MessageDirection, MessageEntry};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_txn::IdGen;

use crate::common::probe_cdr::{ProbeCdr, TerminatedCalls};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const CAROL: &str = "127.0.0.1:5070";
const BOB: &str = "127.0.0.1:5071";
const B2BUA: &str = "127.0.0.1:5080";

/// A bare SUT under `decision`, its ring on so every message's stamp is read.
struct Sut {
    addr: SocketAddr,
    core: B2buaCore,
    terminated: TerminatedCalls,
}

impl Sut {
    async fn spawn(h: &Harness, decision: Arc<dyn CallDecisionEngine>) -> Self {
        Self::spawn_with(h, decision, Arc::new(NoopLimiter), Vec::new()).await
    }

    async fn spawn_with(
        h: &Harness,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
        services: Vec<ServiceDef>,
    ) -> Self {
        let (endpoint, addr) = h
            .bind_sut_with_roles(
                "b2bua",
                B2BUA,
                std::collections::HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            )
            .await;
        let terminated = TerminatedCalls::default();
        let config = B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: addr.ip().to_string(),
            sip_local_port: addr.port(),
            worker_allowed_target_suffixes: vec!["*".into()],
            keepalive_interval_sec: 30,
            keepalive_timeout_sec: 5,
            overload_panic_elu_threshold: 1.1,
            cdr: CdrConfig { message_ring: 32, captured_headers: Vec::new() },
            ..Default::default()
        };
        let deps = B2buaDeps {
            config,
            decision,
            limiter,
            cdr: Arc::new(ProbeCdr::new(terminated.clone())),
            store: Arc::new(InMemoryCallStore::new()),
            store_faults: Default::default(),
            wire_faults: Default::default(),
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            replication: None,
            metrics: B2buaMetrics::new(),
            adaptation_http: None,
            compose: b2bua::rules::ComposeOptions::default(),
        };
        let core = B2buaCore::spawn_with_services(endpoint, deps, services);
        Self { addr, core, terminated }
    }

    /// Every call created is reaped and the one CDR is written.
    async fn assert_reaped(&self) -> Call {
        settle_until(|| self.terminated.snapshot().len() == 1).await;
        settle_until(|| self.core.active_calls() == 0).await;
        assert_eq!(self.core.active_calls(), 0, "the call is removed");
        assert_eq!(self.core.lock_count(), 0, "no stranded per-call lock");
        let m = self.core.metrics();
        assert_eq!(m.creations_total(), m.removals_total(), "every call created is removed");
        let terminated = self.terminated.snapshot();
        assert_eq!(terminated.len(), 1, "exactly one CDR per call");
        terminated.into_iter().next().unwrap()
    }
}

/// `(kind, leg, label)` of every mark, the shape the assertions read.
fn marks(call: &Call) -> Vec<(DecisionKind, Option<&str>, Option<&str>)> {
    call.decision_log.iter().map(|m| (m.kind, m.leg_id.as_deref(), m.label.as_deref())).collect()
}

/// `(direction, method, code, decision_ordinal)` of every ring entry.
fn stamped(entries: &[MessageEntry]) -> Vec<(MessageDirection, &str, Option<u16>, u32)> {
    entries.iter().map(|e| (e.direction, e.method.as_str(), e.code, e.decision_ordinal)).collect()
}

/// `(type, leg, decision_ordinal)` of every CDR event.
fn events(call: &Call) -> Vec<(CdrEventType, &str, u32)> {
    call.cdr_events.iter().map(|e| (e.event_type, e.leg_id.as_str(), e.decision_ordinal)).collect()
}

fn leg<'a>(call: &'a Call, id: &str) -> &'a call::Leg {
    call.b_legs.iter().find(|l| l.leg_id == id).unwrap_or_else(|| panic!("leg {id}"))
}

use MessageDirection::{Authored, Received, Relayed};

/// A routed call whose first leg fails and a failover route replaces it: two
/// marks with their labels, the first leg's messages under ordinal 1, the
/// second leg's and the caller's messages after the reroute under ordinal 2,
/// the events likewise; the caller's INVITE, handled before any decision,
/// under 0.
#[tokio::test(start_paused = true)]
async fn a_rerouted_call_logs_two_labelled_decisions_and_stamps_their_ordinals() {
    let h = Harness::new("decision-log-reroute");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.label = Some("first".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.label = Some("second".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let sut = Sut::spawn(&h, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    carol.receive("INVITE").await.respond(486, "Busy Here").await;
    carol.receive("ACK").await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(
        marks(&done),
        vec![
            (DecisionKind::Route, Some("a"), Some("first")),
            (DecisionKind::FailoverRoute, Some("b-1"), Some("second")),
        ]
    );
    assert_eq!(done.decision_ordinal, 2);
    assert_eq!(done.decision_log[0].ordinal, 1);
    assert_eq!(done.decision_log[1].ordinal, 2);
    assert!(done.decision_log[0].at_ms <= done.decision_log[1].at_ms);

    assert_eq!(
        stamped(&leg(&done, "b-1").messages.entries),
        vec![
            (Relayed, "INVITE", None, 1),
            (Received, "INVITE", Some(486), 1),
            (Authored, "ACK", None, 1),
        ],
        "the first leg lived under the first decision"
    );
    let b2 = stamped(&leg(&done, "b-2").messages.entries);
    assert_eq!(b2[0], (Relayed, "INVITE", None, 2), "the replacement leg's INVITE: {b2:?}");
    assert!(b2.iter().all(|r| r.3 == 2), "everything on the replacement leg: {b2:?}");

    let a = stamped(&done.a_leg.messages.entries);
    assert_eq!(a[0], (Received, "INVITE", None, 0), "the caller's INVITE precedes any decision");
    assert_eq!(a[1], (Authored, "INVITE", Some(100), 0));
    assert_eq!(a[2], (Relayed, "INVITE", Some(180), 2), "the caller's messages after the reroute");
    assert!(a[2..].iter().all(|r| r.3 == 2), "{a:?}");

    let ev = events(&done);
    assert_eq!(ev[0], (CdrEventType::InviteReceived, "a", 0));
    assert_eq!(ev[1], (CdrEventType::InviteSent, "b-1", 1));
    assert_eq!(ev[2], (CdrEventType::Reject, "b-1", 1), "the failure that raised the consult");
    assert!(
        ev[3..].iter().all(|e| e.2 == 2),
        "every event after the reroute is under the second decision: {ev:?}"
    );
    assert!(ev.iter().any(|e| e.0 == CdrEventType::Answer && e.2 == 2));

    // The record projection carries the log as the call holds it.
    let records = sut.core.cdr().read_all().await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision_log, done.decision_log);
    assert_eq!(records[0].events, done.cdr_events);

    let _report = h.finish().await;
}

/// A reject decision is recorded with its label and seeds its `service_ext`
/// exactly as a route does; the final it authors is stamped under it.
#[tokio::test(start_paused = true)]
async fn a_rejected_call_holds_the_reject_label_and_service_ext() {
    let h = Harness::new("decision-log-reject");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                NewCallResponse::Reject(RejectDecision {
                    reject_code: 403,
                    reject_reason: Some("Forbidden".into()),
                    update_headers: None,
                    service_ext: [("svc".to_string(), serde_json::json!({"token": "t-1"}))]
                        .into_iter()
                        .collect(),
                    label: Some("deny".into()),
                })
            })
            .build(),
    );
    let sut = Sut::spawn(&h, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    call.expect(403).await;

    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::Reject, Some("a"), Some("deny"))]);
    assert_eq!(
        done.ext.as_ref().and_then(|e| e.get("svc")),
        Some(&serde_json::json!({"token": "t-1"})),
        "the reject's service slice is on the call"
    );
    // The caller's ACK completes the server transaction (RFC 3261 §17.2.1)
    // and never reaches the call, so the ring ends at the final.
    assert_eq!(
        stamped(&done.a_leg.messages.entries),
        vec![
            (Received, "INVITE", None, 0),
            (Authored, "INVITE", Some(100), 0),
            (Authored, "INVITE", Some(403), 1),
        ]
    );
    assert_eq!(
        events(&done),
        vec![(CdrEventType::InviteReceived, "a", 0), (CdrEventType::Reject, "a", 1)]
    );
    assert!(done.b_legs.is_empty());

    let _report = h.finish().await;
}

/// A decision without a label is marked all the same, `label: None`.
#[tokio::test(start_paused = true)]
async fn a_call_with_no_labels_marks_with_none() {
    let h = Harness::new("decision-log-unlabelled");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut =
        Sut::spawn(&h, Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071))).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(
        done.decision_log,
        vec![DecisionMark {
            ordinal: 1,
            at_ms: done.decision_log[0].at_ms,
            kind: DecisionKind::Route,
            leg_id: Some("a".into()),
            label: None,
        }]
    );
    assert!(leg(&done, "b-1").messages.entries.iter().all(|e| e.decision_ordinal == 1));

    let _report = h.finish().await;
}

/// A limiter that refuses every admission naming `refused` and admits the
/// rest without holds.
struct RefusingLimiter(&'static str);

#[async_trait]
impl CallLimiter for RefusingLimiter {
    async fn admit(&self, entries: &[LimiterEntry]) -> AdmitOutcome {
        match entries.iter().find(|e| e.id == self.0) {
            Some(e) => AdmitOutcome::Rejected { limiter_id: e.id.clone() },
            None => AdmitOutcome::Unavailable,
        }
    }
    async fn release(&self, _holds: &[LimiterHold]) {}
    async fn refresh(&self, holds: &[LimiterHold]) -> Vec<LimiterHold> {
        holds.to_vec()
    }
}

/// A route the limiter refused was never applied: the log holds only the
/// failover route the refusal raised, which answers no failed leg, and the
/// leg it dials is stamped under it.
#[tokio::test(start_paused = true)]
async fn a_limiter_refused_route_marks_nothing_and_the_reroute_marks_once() {
    let h = Harness::new("decision-log-limiter-refused");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.call_limiter = vec![CallLimiterEntry { id: "cap".into(), limit: 1 }];
                r.label = Some("capped".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|req| {
                assert_eq!(req.failure.origin, "call_limiter");
                let mut r = route_to("127.0.0.1", 5071);
                r.label = Some("second".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let sut = Sut::spawn_with(&h, decision, Arc::new(RefusingLimiter("cap")), Vec::new()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::FailoverRoute, None, Some("second"))]);
    assert_eq!(done.b_legs.len(), 1, "the refused route dialed nothing");
    assert!(leg(&done, "b-1").messages.entries.iter().all(|e| e.decision_ordinal == 1));
    assert_eq!(
        events(&done)[..2],
        [(CdrEventType::InviteReceived, "a", 0), (CdrEventType::InviteSent, "b-1", 1)]
    );

    let _report = h.finish().await;
}

/// A scripted engine whose failure consult never answers: the fold is the
/// stack's own resolution.
struct ErringFailover(ScriptedDecisionEngine);

#[async_trait]
impl CallDecisionEngine for ErringFailover {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.0.new_call(req).await
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("down".into()))
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.0.call_refer(req).await
    }
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.0.call_release(req).await
    }
}

/// An unanswered failure consult is no decision: the fold that relays the
/// failure adds no mark, and everything it emits keeps the route's ordinal.
#[tokio::test(start_paused = true)]
async fn an_unanswered_failure_consult_marks_nothing() {
    let h = Harness::new("decision-log-consult-error");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let decision = Arc::new(ErringFailover(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.label = Some("first".into());
                NewCallResponse::Route(r)
            })
            .build(),
    ));
    let sut = Sut::spawn(&h, decision).await;

    let mut call = alice.invite(&carol).with_sdp(OFFER).through(sut.addr).send().await;
    carol.receive("INVITE").await.respond(486, "Busy Here").await;
    carol.receive("ACK").await;
    call.expect(486).await;

    let done = sut.assert_reaped().await;
    assert_eq!(marks(&done), vec![(DecisionKind::Route, Some("a"), Some("first"))]);
    let a = stamped(&done.a_leg.messages.entries);
    assert_eq!(a[2], (Relayed, "INVITE", Some(486), 1), "the relayed failure: {a:?}");
    assert!(a[2..].iter().all(|r| r.3 == 1), "{a:?}");
    assert!(events(&done)[1..].iter().all(|e| e.2 == 1), "{:?}", events(&done));

    let _report = h.finish().await;
}

// ── claimer: a service whose rule takes the failover fold before the core's,
//    as a callflow service does. ─────────────────────────────────────────────
mod claimer {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use sip_message::Method;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub static CLAIMED: AtomicBool = AtomicBool::new(false);

    define_service! {
        id: "claimer",
        machine: CLAIMER,
        states: ClaimerState { Watching },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ClaimerState::Watching.label())),
        rules: [ failover() ],
    }

    /// Dial the fold's destination itself — what the core's fold rule would
    /// have done — so the core rule never runs on this event.
    fn failover() -> RuleDefinition {
        sm_rule! {
            id: "claimer-failover",
            machine: CLAIMER,
            active: [ ClaimerState::Watching ],
            transitions: [],
            effects: [ Effect::Originate { method: Method::Invite, label: "INVITE → replacement" } ],
            matcher: Match::internal_event().topic("call-failure-result").outcome("failover"),
            handle: |ctx: &RuleContext| {
                let CallEvent::InternalEvent { payload, .. } = ctx.event else { return None };
                CLAIMED.store(true, Ordering::SeqCst);
                let dest = payload.get("destination")?;
                let host = dest.get("host")?.as_str()?.to_string();
                let port = dest.get("port")?.as_u64()? as u16;
                Some(RuleHandleResult::new(vec![
                    RuleAction::CreateLeg {
                        destination: (host, port),
                        new_ruri: payload.get("new_ruri").and_then(|v| v.as_str()).map(str::to_string),
                        new_from: None,
                        new_to: None,
                        no_answer_timeout_sec: None,
                        callback_context: None,
                        body_override: None,
                        header_updates: Vec::new(),
                        kind: None,
                    },
                ]))
            },
        }
    }
}

/// The fold is marked where it lands, before any rule reads it: a service
/// rule that claims the failover ahead of the core's still applies it under
/// the decision's mark.
#[tokio::test(start_paused = true)]
async fn a_fold_claimed_by_a_service_rule_is_marked_all_the_same() {
    claimer::CLAIMED.store(false, Ordering::SeqCst);
    let h = Harness::new("decision-log-claimed-fold");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.label = Some("first".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.label = Some("second".into());
                CallTreatment::Route(r)
            })
            .build(),
    );
    let sut =
        Sut::spawn_with(&h, decision, Arc::new(NoopLimiter), vec![claimer::service_def()]).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    carol.receive("INVITE").await.respond(486, "Busy Here").await;
    carol.receive("ACK").await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert!(claimer::CLAIMED.load(Ordering::SeqCst), "the service rule took the fold");
    assert_eq!(
        marks(&done),
        vec![
            (DecisionKind::Route, Some("a"), Some("first")),
            (DecisionKind::FailoverRoute, Some("b-1"), Some("second")),
        ]
    );
    let b2 = stamped(&leg(&done, "b-2").messages.entries);
    assert_eq!(b2[0], (Relayed, "INVITE", None, 2), "{b2:?}");

    let _report = h.finish().await;
}
