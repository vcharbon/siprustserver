//! scenario-harness — the cross-layer scenario test harness (Rust port of
//! `src/test-harness/framework`), plus its report renderers.
//!
//! # Why this is *thin*, not a faithful port of the interpreter
//!
//! The source harness is a fluent DSL (`alice.invite(...).expect(200).ack()`)
//! over a two-phase recorder + a 2300-line interpreter that maintains its own
//! dialog state (CSeq, route sets, tags, offer/answer, forking) and builds its
//! own `trace`, which the reports then render. That dialog/state machinery *is*
//! the transaction + call-context layers — not ported yet — and there is no SUT
//! (B2BUA) to drive against. A faithful interpreter would force porting layers
//! that don't exist, against nothing.
//!
//! It was also written **before** the recording layer. In this port we already
//! have one: `sip-net`'s `RecordingSignalingNetwork` tees every `send_to`/`recv`
//! onto the `layer-harness` `Recorder`. So the design inverts: pseudo agents
//! (`alice`, `bob`) send and receive through the recording-wrapped simulated
//! network, and **the recording is the trace**. The reports are *projected*
//! from the recorded events (`sip_net::to_sip_entries`) + the recorder's lane
//! registry — never from interpreter-maintained state. See ADR-0006 and
//! MIGRATION_PLAN_B2B §4(ii).
//!
//! What is kept: named agents, scenarios-as-data (`Vec<Step>`), the 100 ms-chunk
//! virtual-time advance (via `sip-clock`'s testkit). What is dropped (until the
//! producing layers land): the fluent dialog builder, `or`-branching,
//! `parallel`, media (RTP), and k8s/chaos steps. See MIGRATION_STATUS.md for the
//! per-feature justification.
//!
//! **TEST-ONLY.** This crate composes the recording/audit decorators, which
//! never belong in a production network tree.

pub mod agent;
pub mod anchors;
pub mod callflow;
pub mod dsl;
pub mod egress;
pub mod loadbind;
pub mod realcall;
pub mod report;
pub mod run;

/// Default one-hop transit delay (ms) the harness gives the simulated fabric.
/// A non-zero delay makes the fake network behave like a real one: a sent
/// datagram is delivered `SIMULATED_TRANSIT_DELAY_MS` later, so every recorded
/// message shows `received_ms == sent_ms + delay`. Under a paused runtime the
/// delay is virtual (tokio auto-advances to the delivery timer); under a real
/// runtime it is a real sleep. Override per session with
/// [`Harness::with_transit_delay`].
pub const SIMULATED_TRANSIT_DELAY_MS: u64 = 100;

// The fluent, dialog-aware DSL (auto-generates correct-by-default B2B messages).
// This is the primary surface — scenarios should not hand-author headers.
pub use agent::{
    Agent, CancelHandle, ClientInvite, Dialog, Harness, InDialogRequest, InDialogTxn, Invite,
    OutOfDialogRequest, Proxy, Respond, ServerTxn, StepError,
};
// The Send agent factory for the load-test driver (`crates/loadgen`), plus the
// dependency-light check-verdict projection a sampled page renders.
pub use loadbind::{AgentBinder, CheckNote};
// The low-level scenarios-as-data DSL — for raw/torture cases that must send
// exact (possibly malformed) bytes. `dsl::Agent` is the data-DSL agent handle;
// the fluent `agent::Agent` (re-exported above) is the stateful UA.
pub use anchors::{AnchorKeys, AnchorMsgKind, AnchorTag};
// Imperative call-choreography primitives (the canonical INVITE/180/200/ACK
// dance) — re-exported at the crate root so tests reach for `callflow::establish`
// / `callflow::hangup` / `callflow::Call` without re-typing the handshake.
pub use callflow::{establish, hangup, Call, ANSWER_SDP, OFFER_SDP};
pub use dsl::{AgentId, Match, Scenario, Step};
// The layout-owned egress model (shared with the e2e framework via
// `e2e_model::egress`): how a topology realizes a logical INVITE on its wire.
pub use egress::{ApiCall, CalleeTarget, EgressPolicy, EgressRewrite};
pub use run::{run, ExpectOutcome, RunReport};
// The portable real-call scenarios shared by the load generator and the
// in-process functional leak gate (`realcall::run_asserting` for happy-path
// flows, `realcall::run_collecting` for the voluntarily-failing ones).
pub use realcall::{
    run_asserting, run_collecting, CallCtx, CallEnv, CallScope, CoreIdentity, RealCallScenario,
    ScenarioId,
};
