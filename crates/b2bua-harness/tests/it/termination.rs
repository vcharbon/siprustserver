//! The termination record on the replicated `Call`: who ended the call and
//! why, written once by the first termination, naming the leg whose message
//! or timer caused it and the message-ring cut — the `seq` of the last entry
//! the terminating turn recorded, so what the stack received and sent as part
//! of ending the call (the peer's BYE and its 200, the BYE or CANCEL relayed
//! to the other leg, the 487) is at or under it and everything that came
//! after (the other leg's 200 to that BYE, the ACK to the 487) is above it.
//!
//! The SUT is spawned bare so a probe [`CdrWriter`] hands the test the
//! terminated `Call` with its record, beside the `CdrRecord` it wrote.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter};
use b2bua::config::{B2buaConfig, CdrConfig};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::test_adapter::ReleaseOutcome;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, CallReleaseResponse, CallTreatment, NewCallResponse,
    RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, LimiterHold, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use b2bua_harness::settle_until;
use call::{Call, MessageDirection, MessageEntry, Termination, TerminationCause, TimeoutKind};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_txn::IdGen;

use crate::common::probe_cdr::{ProbeCdr, TerminatedCalls};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const B2BUA: &str = "127.0.0.1:5080";

/// A bare SUT under `decision`, its ring on unless a test turns it off.
struct Sut {
    addr: SocketAddr,
    core: B2buaCore,
    terminated: TerminatedCalls,
    cdr: Arc<ProbeCdr>,
    clock: Clock,
}

impl Sut {
    async fn spawn(h: &Harness) -> Self {
        Self::spawn_tuned(
            h,
            Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
            |_| {},
        )
        .await
    }

    async fn spawn_tuned(
        h: &Harness,
        decision: Arc<dyn CallDecisionEngine>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::spawn_with(h, decision, Arc::new(NoopLimiter), tune).await
    }

    async fn spawn_with(
        h: &Harness,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        let (endpoint, addr) = h
            .bind_sut_with_roles(
                "b2bua",
                B2BUA,
                std::collections::HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            )
            .await;
        let terminated = TerminatedCalls::default();
        let mut config = B2buaConfig {
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
        tune(&mut config);
        let cdr = Arc::new(ProbeCdr::new(terminated.clone()));
        let clock = Clock::test_at(0);
        let deps = B2buaDeps {
            config,
            decision,
            limiter,
            cdr: cdr.clone(),
            store: Arc::new(InMemoryCallStore::new()),
            store_faults: Default::default(),
            wire_faults: Default::default(),
            clock: clock.clone(),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            replication: None,
            metrics: B2buaMetrics::new(),
            adaptation_http: None,
            compose: b2bua::rules::ComposeOptions::default(),
        };
        let core = B2buaCore::spawn(endpoint, deps);
        Self { addr, core, terminated, cdr, clock }
    }

    fn live(&self, call_id: &str, from_tag: &str) -> Call {
        let call_ref = call::derive_call_ref("w0", call_id, from_tag);
        self.core.live_call(&call_ref).expect("the call is live")
    }

    /// Every call created is reaped and the one CDR is written: the
    /// terminated `Call` and the record built from it.
    async fn assert_reaped(&self) -> (Call, CdrRecord) {
        settle_until(|| self.terminated.snapshot().len() == 1).await;
        settle_until(|| self.core.active_calls() == 0).await;
        assert_eq!(self.core.active_calls(), 0, "the call is removed");
        assert_eq!(self.core.lock_count(), 0, "no stranded per-call lock");
        let m = self.core.metrics();
        assert_eq!(m.creations_total(), m.removals_total(), "every call created is removed");
        let terminated = self.terminated.snapshot();
        assert_eq!(terminated.len(), 1, "exactly one CDR per call");
        let records = self.cdr.read_all().await;
        assert_eq!(records.len(), 1, "exactly one record per call");
        (terminated.into_iter().next().unwrap(), records.into_iter().next().unwrap())
    }
}

/// The record, which the call carries and the CDR restates.
fn record(call: &Call, cdr: &CdrRecord) -> Termination {
    let t = call.termination.clone().expect("the terminated call carries its record");
    assert_eq!(cdr.termination.as_ref(), Some(&t), "the CDR restates the call's record");
    t
}

fn b_leg(call: &Call) -> &call::Leg {
    assert_eq!(call.b_legs.len(), 1, "one b-leg");
    &call.b_legs[0]
}

/// The one entry of `entries` with `direction`, `method` and `code`.
fn entry<'a>(
    entries: &'a [MessageEntry],
    direction: MessageDirection,
    method: &str,
    code: Option<u16>,
) -> &'a MessageEntry {
    let found: Vec<&MessageEntry> = entries
        .iter()
        .filter(|e| e.direction == direction && e.method == method && e.code == code)
        .collect();
    assert_eq!(found.len(), 1, "one {direction:?} {method} {code:?} entry: {found:?}");
    found[0]
}

use MessageDirection::{Authored, Received, Relayed};

/// The caller hangs up: the record names the caller's BYE, and the cut is the
/// BYE this stack relayed to the callee — the caller's BYE and its 200 are
/// under it, the callee's 200 to the relayed BYE above it.
#[tokio::test(start_paused = true)]
async fn a_caller_bye_is_recorded_by_the_caller_and_cut_at_the_relayed_bye() {
    let h = Harness::new("termination-caller-bye");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(5)).await;
    let sent_at = sut.clock.now_ms();

    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bye.expect(200).await;
    h.advance(Duration::from_secs(1)).await;
    bob_bye.respond(200, "OK").await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    let callers_bye = entry(a, Received, "BYE", None);
    assert_eq!(t.at_ms, callers_bye.at_ms, "the clock of the turn that took the BYE");
    assert!(t.at_ms >= sent_at, "after the BYE left the caller");
    let relayed_bye = entry(b, Authored, "BYE", None);
    assert_eq!(t.last_seq, relayed_bye.seq, "cut at the BYE relayed to the callee");
    assert!(entry(a, Received, "BYE", None).seq <= t.last_seq, "the caller's BYE is in");
    assert!(entry(a, Authored, "BYE", Some(200)).seq <= t.last_seq, "its 200 is in");
    assert!(entry(b, Received, "BYE", Some(200)).seq > t.last_seq, "the callee's 200 is after");
    // Every entry of the call before the cut precedes every entry after it.
    let all: Vec<&MessageEntry> = a.iter().chain(b.iter()).collect();
    assert_eq!(all.iter().filter(|e| e.seq > t.last_seq).count(), 1, "one entry after the cut");

    let _report = h.finish().await;
}

/// The callee hangs up: the mirror image — by the callee, cut at the BYE
/// relayed to the caller, the caller's 200 to it after.
#[tokio::test(start_paused = true)]
async fn a_callee_bye_is_recorded_by_the_callee_and_cut_at_the_relayed_bye() {
    let h = Harness::new("termination-callee-bye");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();
    h.advance(Duration::from_secs(5)).await;

    let mut bye = bob_dialog.bye().await;
    let mut alice_bye = alice.receive("BYE").await;
    bye.expect(200).await;
    h.advance(Duration::from_secs(1)).await;
    alice_bye.respond(200, "OK").await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert_eq!(
        t.last_seq,
        entry(a, Authored, "BYE", None).seq,
        "cut at the BYE relayed to the caller"
    );
    assert!(entry(b, Received, "BYE", None).seq <= t.last_seq, "the callee's BYE is in");
    assert!(entry(b, Authored, "BYE", Some(200)).seq <= t.last_seq, "its 200 is in");
    assert!(entry(a, Received, "BYE", Some(200)).seq > t.last_seq, "the caller's 200 is after");

    let _report = h.finish().await;
}

/// The caller CANCELs a ringing call: the record names the caller's CANCEL,
/// and the cut covers the CANCEL, the 200 and 487 the layer answered it with
/// and the CANCEL sent to the callee; the callee's 200 and 487 and the ACK
/// to it are after.
#[tokio::test(start_paused = true)]
async fn a_caller_cancel_is_recorded_by_the_caller_and_cut_at_the_relayed_cancel() {
    let h = Harness::new("termination-caller-cancel");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    h.advance(Duration::from_secs(1)).await;
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteCancel);
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert_eq!(t.last_seq, entry(b, Authored, "CANCEL", None).seq, "cut at the CANCEL sent");
    assert!(entry(a, Received, "CANCEL", None).seq <= t.last_seq);
    assert!(entry(a, Authored, "CANCEL", Some(200)).seq <= t.last_seq);
    assert!(entry(a, Authored, "INVITE", Some(487)).seq <= t.last_seq);
    assert!(entry(b, Received, "CANCEL", Some(200)).seq > t.last_seq);
    assert!(entry(b, Received, "INVITE", Some(487)).seq > t.last_seq);
    assert!(entry(b, Authored, "ACK", None).seq > t.last_seq);

    let _report = h.finish().await;
}

/// The duration cap ends an established call: no leg caused it, and both
/// BYEs this stack sends are under the cut, both 200s after.
#[tokio::test(start_paused = true)]
async fn a_max_duration_end_is_recorded_by_no_leg_and_cut_after_both_byes() {
    let h = Harness::new("termination-max-duration");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut route = route_to("127.0.0.1", 5070);
                route.features.platform.max_duration_sec = 20;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    let sut = Sut::spawn_tuned(&h, decision, |_| {}).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(21)).await;
    let mut alice_bye = alice.receive("BYE").await;
    let mut bob_bye = bob.receive("BYE").await;
    h.advance(Duration::from_secs(1)).await;
    alice_bye.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::MaxDuration);
    assert_eq!(t.by_leg, None);
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert!(entry(a, Authored, "BYE", None).seq <= t.last_seq, "the caller's BYE is in");
    assert!(entry(b, Authored, "BYE", None).seq <= t.last_seq, "the callee's BYE is in");
    assert!(entry(a, Received, "BYE", Some(200)).seq > t.last_seq);
    assert!(entry(b, Received, "BYE", Some(200)).seq > t.last_seq);
    assert_eq!(
        t.last_seq,
        entry(a, Authored, "BYE", None).seq.max(entry(b, Authored, "BYE", None).seq),
        "cut at the last BYE sent"
    );

    let _report = h.finish().await;
}

/// The setup deadline ends a call still ringing: the 408 to the caller and
/// the CANCEL to the callee are under the cut, the callee's answers and the
/// ACK after.
#[tokio::test(start_paused = true)]
async fn a_setup_timeout_is_recorded_as_a_setup_deadline() {
    let h = Harness::new("termination-setup-timeout");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(
        &h,
        Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
        |c| c.setup_timeout_sec = 10,
    )
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    h.advance(Duration::from_secs(11)).await;
    call.expect(408).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    h.advance(Duration::from_secs(1)).await;
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::Timeout(TimeoutKind::Setup));
    assert_eq!(t.by_leg, None);
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert!(entry(a, Authored, "INVITE", Some(408)).seq <= t.last_seq, "the 408 is in");
    assert_eq!(t.last_seq, entry(b, Authored, "CANCEL", None).seq, "cut at the CANCEL sent");
    assert!(entry(b, Received, "CANCEL", Some(200)).seq > t.last_seq);
    assert!(entry(b, Received, "INVITE", Some(487)).seq > t.last_seq);
    assert!(entry(b, Authored, "ACK", None).seq > t.last_seq);

    let _report = h.finish().await;
}

/// The decision layer refuses the call: the record names the decision, no
/// leg, and the cut is the final sent — the last message of the call.
#[tokio::test(start_paused = true)]
async fn a_decision_reject_is_recorded_and_cut_at_the_final_sent() {
    let h = Harness::new("termination-decision-reject");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                NewCallResponse::Reject(RejectDecision {
                    reject_code: 403,
                    reject_reason: Some("Forbidden".into()),
                    update_headers: None,
                    service_ext: Default::default(),
                    label: None,
                })
            })
            .build(),
    );
    let sut = Sut::spawn_tuned(&h, decision, |_| {}).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    call.expect(403).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::DecisionReject);
    assert_eq!(t.by_leg, None);
    let a = &done.a_leg.messages.entries;
    assert_eq!(t.last_seq, entry(a, Authored, "INVITE", Some(403)).seq, "cut at the 403");
    assert_eq!(t.last_seq, done.message_seq, "nothing came after");
    // The JSON record states the cause in snake_case.
    let json = serde_json::to_value(&cdr).unwrap();
    assert_eq!(json["termination"]["cause"], serde_json::json!("decision_reject"));
    assert_eq!(json["termination"]["by_leg"], serde_json::Value::Null);
    assert_eq!(json["termination"]["last_seq"], serde_json::json!(t.last_seq));

    let _report = h.finish().await;
}

/// The callee's final ends a call the decision layer does not reroute: the
/// record names the callee, and the cut covers its final, the hop ACK and
/// the final relayed to the caller.
#[tokio::test(start_paused = true)]
async fn a_callee_final_with_no_reroute_is_recorded_by_the_callee() {
    let h = Harness::new("termination-callee-final");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    call.expect(486).await;
    h.advance(Duration::from_secs(1)).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteFinal);
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert!(entry(b, Received, "INVITE", Some(486)).seq <= t.last_seq);
    assert!(entry(b, Authored, "ACK", None).seq <= t.last_seq);
    assert_eq!(t.last_seq, entry(a, Relayed, "INVITE", Some(486)).seq, "cut at the relayed 486");
    assert_eq!(t.last_seq, done.message_seq, "nothing came after");
    let json = serde_json::to_value(&cdr).unwrap();
    assert_eq!(json["termination"]["cause"], serde_json::json!("remote_final"));
    assert_eq!(json["termination"]["by_leg"], serde_json::json!("b-1"));

    let _report = h.finish().await;
}

/// A leg that answers no liveness probe ends the call under a keepalive
/// deadline, by that leg; the BYEs to both peers are under the cut.
#[tokio::test(start_paused = true)]
async fn a_keepalive_timeout_is_recorded_by_the_silent_leg() {
    let h = Harness::new("termination-keepalive");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    h.advance(Duration::from_secs(30)).await;
    alice.receive("OPTIONS").await.respond(200, "OK").await;
    let _silent = bob.receive("OPTIONS").await;
    h.advance(Duration::from_secs(5)).await;
    let mut alice_bye = alice.receive("BYE").await;
    let mut bob_bye = bob.receive_tolerating("BYE", &["OPTIONS"]).await;
    h.advance(Duration::from_secs(1)).await;
    alice_bye.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::Timeout(TimeoutKind::Keepalive));
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    let b = &done.b_legs[0].messages.entries;
    assert_eq!(t.last_seq, entry(b, Authored, "BYE", None).seq, "cut at the BYE to the callee");
    assert!(entry(a, Authored, "BYE", None).seq < t.last_seq, "the BYE to the caller is in");
    assert!(entry(a, Received, "BYE", Some(200)).seq > t.last_seq);
    assert!(entry(b, Received, "BYE", Some(200)).seq > t.last_seq);
    let json = serde_json::to_value(&cdr).unwrap();
    assert_eq!(json["termination"]["cause"], serde_json::json!({"timeout": "keepalive"}));

    let _report = h.finish().await;
}

/// With the ring off the record is written all the same and its cut stays
/// `0`: there is no sequence to cut.
#[tokio::test(start_paused = true)]
async fn the_ring_off_leaves_the_cut_at_zero() {
    let h = Harness::new("termination-ring-off");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(
        &h,
        Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
        |c| c.cdr = CdrConfig::default(),
    )
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteBye);
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    assert_eq!(t.last_seq, 0);
    assert_eq!(done.message_seq, 0);

    let _report = h.finish().await;
}

/// A limiter that refuses every admission naming `refused`.
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

/// A route to bob ringing at most `no_answer_sec`, consulted on failure when
/// `consult` (a callback context).
fn ringing_route(no_answer_sec: i64, consult: bool) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(no_answer_sec);
                r.callback_context = consult.then(|| "ctx".to_string());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Relay { label: Some("let-stand".into()) })
            .build(),
    )
}

/// A route to bob under a 20 s cap, the max-call-duration event subscribed and
/// `release` scripting the consult.
fn released_route(
    release: impl Fn() -> ReleaseOutcome + Send + Sync + 'static,
) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_sec = 20;
                r.callback_context = Some("ctx".into());
                r.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
                NewCallResponse::Route(r)
            })
            .on_release(move |_| release())
            .build(),
    )
}

/// Ring bob, let the no-answer deadline pass, and complete the CANCEL round.
async fn ring_past_no_answer(
    h: &Harness,
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    sut: &Sut,
    final_status: u16,
) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    h.advance(Duration::from_secs(11)).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    call.expect(final_status).await;
    h.advance(Duration::from_secs(1)).await;
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;
}

/// Establish alice↔bob and let the 20 s cap expire; both legs are BYE'd.
async fn establish_past_cap(
    h: &Harness,
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    sut: &Sut,
) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(21)).await;
    let mut alice_bye = alice.receive("BYE").await;
    let mut bob_bye = bob.receive("BYE").await;
    h.advance(Duration::from_secs(1)).await;
    alice_bye.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;
}

/// A callee that never answers, with no failure consult: the caller's 480 is
/// authored in the terminating turn, so it precedes the cut with the CANCEL.
#[tokio::test(start_paused = true)]
async fn a_no_answer_with_no_consult_answers_the_caller_under_the_cut() {
    let h = Harness::new("termination-no-answer");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(&h, ringing_route(10, false), |_| {}).await;

    ring_past_no_answer(&h, &alice, &bob, &sut, 480).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::Timeout(TimeoutKind::NoAnswer));
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    let b = &b_leg(&done).messages.entries;
    assert!(entry(a, Authored, "INVITE", Some(480)).seq <= t.last_seq, "the 480 is in");
    assert!(entry(b, Authored, "CANCEL", None).seq <= t.last_seq, "the CANCEL is in");
    assert!(entry(b, Received, "INVITE", Some(487)).seq > t.last_seq);
    assert!(entry(b, Authored, "ACK", None).seq > t.last_seq);

    let _report = h.finish().await;
}

/// A failure consult that lets the callee's final stand: the record names the
/// callee's final, by the failed leg, the relayed final under the cut.
#[tokio::test(start_paused = true)]
async fn a_failure_let_stand_with_a_final_is_the_callees() {
    let h = Harness::new("termination-let-stand-final");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(&h, ringing_route(60, true), |_| {}).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    bob.receive("INVITE").await.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    call.expect(486).await;
    h.advance(Duration::from_secs(1)).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::RemoteFinal);
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    assert_eq!(t.last_seq, entry(a, Relayed, "INVITE", Some(486)).seq, "cut at the relayed 486");
    assert_eq!(done.decision_log.len(), 2, "the route and the relay decision");

    let _report = h.finish().await;
}

/// A failure consult that lets a no-answer stand: the record names the
/// deadline, by the silent leg, and the caller's 480 is authored in the
/// terminating turn, under the cut.
#[tokio::test(start_paused = true)]
async fn a_failure_let_stand_after_no_answer_is_the_deadline() {
    let h = Harness::new("termination-let-stand-no-answer");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(&h, ringing_route(10, true), |_| {}).await;

    ring_past_no_answer(&h, &alice, &bob, &sut, 480).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::Timeout(TimeoutKind::NoAnswer));
    assert_eq!(t.by_leg.as_deref(), Some("b-1"));
    let a = &done.a_leg.messages.entries;
    assert!(entry(a, Authored, "INVITE", Some(480)).seq <= t.last_seq, "the 480 is in");

    let _report = h.finish().await;
}

/// The decision layer answers a release consult with a release: the layer
/// ended the call.
#[tokio::test(start_paused = true)]
async fn a_release_decision_ends_the_call_under_the_decision() {
    let h = Harness::new("termination-release-decided");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = released_route(|| {
        ReleaseOutcome::Respond(CallReleaseResponse::Release { label: Some("hangup".into()) })
    });
    let sut = Sut::spawn_tuned(&h, decision, |_| {}).await;

    establish_past_cap(&h, &alice, &bob, &sut).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::DecisionRelease);
    assert_eq!(t.by_leg, None);

    let _report = h.finish().await;
}

/// The decision layer answers a release consult with a route the limiter
/// refuses: the layer ended the call all the same.
#[tokio::test(start_paused = true)]
async fn a_limiter_refused_reroute_ends_the_call_under_the_decision() {
    let h = Harness::new("termination-release-limiter-refused");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = released_route(|| {
        let mut r = route_to("127.0.0.1", 5071);
        r.call_limiter = vec![CallLimiterEntry { id: "cap".into(), limit: 1 }];
        ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
    });
    let sut = Sut::spawn_with(&h, decision, Arc::new(RefusingLimiter("cap")), |_| {}).await;

    establish_past_cap(&h, &alice, &bob, &sut).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::DecisionRelease);
    assert_eq!(t.by_leg, None);

    let _report = h.finish().await;
}

/// A release consult the engine cannot answer: the cap that raised it ended
/// the call.
#[tokio::test(start_paused = true)]
async fn an_unanswered_release_consult_ends_the_call_under_the_cap() {
    let h = Harness::new("termination-release-engine-error");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(&h, released_route(|| ReleaseOutcome::Error), |_| {}).await;

    establish_past_cap(&h, &alice, &bob, &sut).await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t.cause, TerminationCause::MaxDuration);
    assert_eq!(t.by_leg, None);

    let _report = h.finish().await;
}

/// The first record stands: a relayed BYE the callee never answers reaches
/// the teardown's own deadlines — the transaction's Timer F and the safety
/// timer both terminate again — and the record still names the caller's BYE
/// and its cut.
#[tokio::test(start_paused = true)]
async fn a_termination_ended_again_by_the_safety_timer_keeps_the_first_record() {
    let h = Harness::new("termination-written-once");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    let _unanswered = bob.receive("BYE").await;
    bye.expect(200).await;
    let terminating = sut.live(&call.call_id(), dialog.local_tag());
    let first = terminating.termination.clone().expect("recorded at the BYE");
    assert_eq!(first.cause, TerminationCause::RemoteBye);
    // The relayed BYE is retransmitted and gives up; the safety timer reaps.
    h.advance(Duration::from_secs(40)).await;
    bob.drain().await;

    let (done, cdr) = sut.assert_reaped().await;
    let t = record(&done, &cdr);
    assert_eq!(t, first, "the first record stands, cut and all");
    assert_eq!(t.by_leg.as_deref(), Some("a"));
    let b = &b_leg(&done).messages.entries;
    assert_eq!(t.last_seq, entry(b, Authored, "BYE", None).seq);

    let _report = h.finish().await;
}
