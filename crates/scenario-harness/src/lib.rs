//! scenario-harness — the cross-layer scenario test harness, plus its report
//! renderers.
//!
//! **Recording-first** (ADR-0006): agents (`alice`, `bob`) send and receive
//! through the recording-wrapped simulated network — `sip-net`'s
//! `RecordingSignalingNetwork` tees every `send_to`/`recv` onto the
//! `layer-harness` `Recorder` — and **the recording is the trace**. The
//! reports are *projected* from the recorded events
//! (`sip_net::to_sip_entries`) + the recorder's lane registry, never from
//! harness-maintained state.
//!
//! **TEST-ONLY.** This crate composes the recording/audit decorators, which
//! never belong in a production network tree.

pub mod actor;
pub mod agent;
pub mod anchors;
pub mod callee_group;
pub mod callflow;
pub mod claim;
pub mod dsl;
pub mod egress;
pub mod legpick;
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
    Agent, CancelHandle, ClientInvite, ClientReinvite, Dialog, Harness, InDialogRequest,
    InDialogTxn, Inbound, Invite, OutOfDialogRequest, Proxy, Respond, ServerTxn, StepError,
};
// Multi-callee routing: several logical agents on one bound socket, demuxed by
// the shared R-URI leg-picker (the transfer Bob/Charlie/David fabric).
pub use callee_group::{CalleeGroup, CalleeGroupBuilder};
// Data-driven inbound-leg claims: the runtime (per-call-instance) counterpart
// of the compiled LegPicker, consumed by the load mux's claim demux tier.
pub use claim::{resolve_claim, ClaimRule};
pub use legpick::{labelled_prefix_leg_picker, prefix_leg_picker, LegInfo, LegPicker};
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
// The per-call environment shared by the load generator and the in-process
// functional leak gate, plus the actor-scenario runners
// (`realcall::run_actor_asserting` for happy-path flows,
// `realcall::run_actor_collecting` for the voluntarily-failing ones).
pub use realcall::{
    run_actor_asserting, run_actor_collecting, CallCtx, CallEnv, CallScope, Challenge,
    ChallengeResponder, CoreIdentity, ScenarioId,
};
