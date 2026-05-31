//! The driver — the thin "interpreter" (port of the load-bearing core of
//! `interpreter.ts`, MIGRATION_PLAN_B2B §4(ii)'s `runDriveOnly` equivalent).
//!
//! Unlike the source interpreter, this driver maintains **no trace and no
//! dialog state**. Its only job is to bind each agent on the
//! recording-wrapped simulated [`SignalingNetwork`] and replay the step list.
//! Every `send_to` / `recv` flows through the recording decorator, so the
//! `layer-harness` `Recorder` *is* the trace — the reports are projected from
//! its channel snapshot afterwards (`sip_net::to_sip_entries`), exactly the
//! "have pseudo alice and bob do the recording" design.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{NetworkTag, RecordedScenario, Recorder, RunContext, Stamped, TransportKind};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{
    to_sip_entries, with_all_contracts, BindUdpOpts, RecordedSipEntry, ScopedAuditOptions,
    SignalingAuditViolation, SignalingNetworkEvent, SimulatedSignalingNetwork, UdpEndpoint,
};

use crate::dsl::{Match, Scenario, Step};

/// Default per-`Expect` wait. Generous because the simulated fabric delivers
/// via a spawned task; a real failure trips long before this.
const EXPECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The outcome of one `Expect` step.
#[derive(Clone, Debug)]
pub struct ExpectOutcome {
    pub agent: String,
    /// What was asked for (`Match::describe`).
    pub expected: String,
    pub passed: bool,
    /// What happened — the matched summary, or the reason it failed.
    pub detail: String,
}

/// Everything a run produced. The recording is the source of truth: call
/// [`RunReport::entries`] for the wire trace and [`RunReport::scenario`] for
/// lanes + anomalies.
pub struct RunReport {
    pub scenario_name: String,
    pub description: Option<String>,
    pub expects: Vec<ExpectOutcome>,
    /// The audit verdict from the recording layer's `close()` (informational;
    /// a basic harness run does not fail on it).
    pub audit: Result<(), SignalingAuditViolation>,
    recorder: Recorder,
    events: Vec<Stamped<SignalingNetworkEvent>>,
}

impl RunReport {
    /// `true` when every `Expect` matched.
    pub fn passed(&self) -> bool {
        self.expects.iter().all(|e| e.passed)
    }

    /// The wire trace, projected from the recording channel.
    pub fn entries(&self) -> Vec<RecordedSipEntry> {
        to_sip_entries(&self.events)
    }

    /// The recorder's drained scenario state (lanes + anomalies).
    pub fn scenario(&self) -> RecordedScenario {
        self.recorder.snapshot()
    }

    /// Raw recorded events, for tests that assert on the channel directly.
    pub fn events(&self) -> &[Stamped<SignalingNetworkEvent>] {
        &self.events
    }
}

fn matches(msg: &SipMessage, matcher: &Match) -> bool {
    match (msg, matcher) {
        (SipMessage::Request(r), Match::Method(m)) => r.method.eq_ignore_ascii_case(m),
        (SipMessage::Response(r), Match::Status(s)) => r.status == *s,
        (_, Match::Any) => true,
        _ => false,
    }
}

fn summarize(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!("{} {}", r.method, r.uri),
        SipMessage::Response(r) => format!("{} {} ({})", r.status, r.reason, r.cseq.method),
    }
}

/// Run a scenario end to end and return its [`RunReport`].
///
/// Binds each agent on `paranoidInputs(scopedAudit(SimulatedSignalingNetwork))`
/// (the canonical contract stack), registers a lane per agent so the reports
/// carry names, replays the steps, then closes the recording layer.
pub async fn run(scenario: &Scenario) -> RunReport {
    // Timestamps ride a monotonic-anchored `Clock` constructed *inside* the
    // runtime, so under `#[tokio::test(start_paused = true)]` the recorded
    // `at_ms` (and thus the report's relative-time labels) advance in lockstep
    // with `tokio::time::advance` / the `Advance` step's 100 ms chunks. Anchor
    // at 0 → the first event sits at `T+0.000s`. See sip-clock crate docs.
    let recorder = Recorder::with_clock(TransportKind::Fake, Clock::test_at(0));
    let sim = Arc::new(SimulatedSignalingNetwork::new(0));
    let wrapped = with_all_contracts(
        sim,
        recorder.clone(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );

    // Bind one endpoint per agent and register its lane (name → address) so
    // the renderer labels columns instead of falling back to bare ip:port.
    let mut endpoints: HashMap<usize, Box<dyn UdpEndpoint>> = HashMap::new();
    for (i, agent) in scenario.agents.iter().enumerate() {
        recorder.register_lane(agent.addr, agent.name.clone(), NetworkTag::Ext);
        let ep = wrapped
            .network
            .bind_udp(BindUdpOpts::new(agent.addr, 64))
            .await
            .unwrap_or_else(|e| panic!("bind {} failed: {e}", agent.addr));
        endpoints.insert(i, ep);
    }

    let parser = CustomParser::new();
    let mut expects = Vec::new();

    for step in &scenario.steps {
        match step {
            Step::Send { from, to, raw } => {
                let dst = scenario.agent_at(*to).addr;
                endpoints[&from.0]
                    .send_to(raw, dst)
                    .await
                    .unwrap_or_else(|e| panic!("send failed: {e}"));
            }
            Step::Expect { agent, matcher } => {
                let outcome = run_expect(endpoints[&agent.0].as_ref(), scenario.agent_at(*agent), matcher, &parser).await;
                expects.push(outcome);
            }
            Step::Advance { ms } => {
                // Requires a paused runtime; the 100 ms chunking mirrors the
                // source so in-flight delivery tasks observe intermediate time.
                sip_clock::testkit::advance_in_100ms_chunks(Duration::from_millis(*ms)).await;
            }
        }
    }

    // Capture the channel before dropping endpoints (drop only appends
    // BindRelease, which the projection ignores), then close the layer.
    let events = wrapped.recording.channel().snapshot();
    drop(endpoints);
    let audit = wrapped.recording.close().await;

    RunReport {
        scenario_name: scenario.name.clone(),
        description: scenario.description.clone(),
        expects,
        audit,
        recorder,
        events,
    }
}

async fn run_expect(
    endpoint: &dyn UdpEndpoint,
    agent: &crate::dsl::Agent,
    matcher: &Match,
    parser: &CustomParser,
) -> ExpectOutcome {
    let expected = matcher.describe();
    match tokio::time::timeout(EXPECT_TIMEOUT, endpoint.recv()).await {
        Err(_) => ExpectOutcome {
            agent: agent.name.clone(),
            expected,
            passed: false,
            detail: format!("timed out after {EXPECT_TIMEOUT:?} waiting for a datagram"),
        },
        Ok(None) => ExpectOutcome {
            agent: agent.name.clone(),
            expected,
            passed: false,
            detail: "endpoint queue closed".to_string(),
        },
        Ok(Some(pkt)) => match parser.parse(&pkt.raw) {
            Ok(msg) => {
                let passed = matches(&msg, matcher);
                ExpectOutcome {
                    agent: agent.name.clone(),
                    expected,
                    passed,
                    detail: if passed {
                        format!("received {}", summarize(&msg))
                    } else {
                        format!("received {} (no match)", summarize(&msg))
                    },
                }
            }
            Err(e) => ExpectOutcome {
                agent: agent.name.clone(),
                expected,
                passed: false,
                detail: format!("unparseable datagram: {e}"),
            },
        },
    }
}
