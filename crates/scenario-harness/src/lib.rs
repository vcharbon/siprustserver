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

pub mod dsl;
pub mod report;
pub mod run;

pub use dsl::{Agent, AgentId, Match, Scenario, Step};
pub use run::{run, ExpectOutcome, RunReport};
