//! End-to-end: a traced call records the DATAGRAM it received, and a decision
//! round trip records when it actually started and ended (ADR-0026).
//!
//! Two properties, one complete callflow.
//!
//! **Raw wire bytes.** A `sip.in` event exists to diagnose what a peer sent —
//! including the things the lenient parser normalizes away. Both intake paths
//! (the initial INVITE's backfill and the in-dialog guarded record) therefore
//! carry the received datagram, not a re-serialization of the parse. The probe
//! is a header value with padding after the colon: it survives on the wire and
//! is trimmed by any round trip through the parser, so a byte-exact record is
//! the only way it shows up.
//!
//! **Round-trip timing.** The `/call/new` child span's request and response are
//! separate facts at separate times. Stamping both with the handler's entry
//! timestamp renders every decision consult as zero-length — exactly the
//! measurement an operator opens the trace for. The decision here parks for a
//! known virtual duration, so the recorded span must be that long.
//!
//! The trace registry is process-wide (one root span per call per process), so
//! this file holds exactly ONE test and installs its own gate.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_harness::{settle_until, B2buaSut};
use observe::{RateDraw, SampleAdmission, TokenBucket};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The fidelity probe: a value written with an extra space after the colon. The
/// serializer emits `<name>: <value>`, so the wire carries two spaces; the
/// parser trims the value, so re-serializing the parse carries one.
const PROBE: &str = "X-Trace-Fidelity";
const PROBE_INVITE: &str = " padded-invite";
const PROBE_BYE: &str = " padded-bye";

/// How long `/call/new` parks. Virtual time under the paused runtime, and well
/// clear of one transit hop so the two ends of the round trip cannot coincide.
const DECISION_DELAY: Duration = Duration::from_secs(2);

/// A decision engine that parks before delegating — the stand-in for a real
/// `/call/new` HTTP round trip. Only `new_call` is delayed.
struct SlowDecisionEngine {
    inner: Arc<dyn CallDecisionEngine>,
    delay: Duration,
}

#[async_trait]
impl CallDecisionEngine for SlowDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(self.delay).await;
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

/// A gate that samples every call.
fn sample_everything() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        false,
    )));
}

/// One captured field of one captured event.
fn field(event: &observe::CapturedEvent, name: &str) -> String {
    event
        .fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("event has no `{name}` field: {}", event.line()))
}

#[tokio::test(start_paused = true)]
async fn a_traced_call_records_the_datagram_and_the_length_of_its_decision() {
    sample_everything();
    let (_log_guard, log) = observe::test_buffer();

    let h = Harness::new("b2bua-trace-wire-fidelity");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let decision = Arc::new(SlowDecisionEngine {
        inner: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5072)),
        delay: DECISION_DELAY,
    });
    let b2bua = B2buaSut::builder(decision)
        // The decision is slow but SUCCESSFUL: keep the ADR-0022 deadline clear
        // of the park (the default 5000 ms would be a race, not a scenario).
        .tune(|c| c.call_control_timeout_ms = 20_000)
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    // ── The complete callflow, hand-rolled because the INVITE carries the probe ─
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header(PROBE, PROBE_INVITE)
        .through(b2bua.addr)
        .send()
        .await;
    // The handler parks on the decision; advance past it so the b-leg is built.
    h.advance(DECISION_DELAY + Duration::from_secs(1)).await;

    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // alice hangs up with the probe on the BYE — an IN-DIALOG message, so it
    // rides the router's guarded `sip.in` rather than the intake backfill.
    let mut bye =
        dialog.send_request(InDialogMethod::Bye).with_header(PROBE, PROBE_BYE).send().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "the scenario is a complete call");

    // ── Raw wire bytes, both intake paths ───────────────────────────────────
    let padded_invite = format!("{PROBE}:  {}", PROBE_INVITE.trim());
    let padded_bye = format!("{PROBE}:  {}", PROBE_BYE.trim());
    let recorded: Vec<String> =
        log.matching("kind=sip.in").iter().map(|e| field(e, "body")).collect();
    assert!(
        recorded.iter().any(|b| b.starts_with("INVITE ") && b.contains(&padded_invite)),
        "the INVITE is recorded as the datagram that arrived, not as a re-render \
         of the parse: {recorded:?}"
    );
    assert!(
        recorded.iter().any(|b| b.starts_with("BYE ") && b.contains(&padded_bye)),
        "…and so is an in-dialog request: {recorded:?}"
    );

    // ── The decision round trip has a length ────────────────────────────────
    let requests = log.matching("kind=http.request");
    let responses = log.matching("kind=http.response");
    let new_call = requests
        .iter()
        .find(|e| e.contains("detail=/call/new"))
        .expect("the decision consult is a child span");
    let answered = responses.first().expect("with both bodies recorded");
    let sent: i64 = field(new_call, "at_ms").parse().expect("at_ms is epoch ms");
    let received: i64 = field(answered, "at_ms").parse().expect("at_ms is epoch ms");
    let parked = DECISION_DELAY.as_millis() as i64;
    // The park is the floor; the ceiling allows the `Harness::advance` chunk the
    // paused clock is driven at, since the sleep expires inside one. A single
    // shared timestamp renders every consult as instant and fails the floor.
    assert!(
        (parked..=parked + 200).contains(&(received - sent)),
        "the round trip is recorded from when the request left ({sent}) to when \
         the response landed ({received}); the decision parked for {parked} ms",
    );

    b2bua.assert_fully_reaped();
    assert_eq!(traces().active(), 0, "the root span closed with the call");

    let _report = h.finish().await;
}
