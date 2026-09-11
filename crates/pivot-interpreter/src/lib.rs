//! pivot-interpreter — the pivot v3 scenario interpreter.
//!
//! **Compile once, run many** (`PCAP2TEST_PIVOT_V3.md` §14): a document
//! compiles to an immutable [`Plan`](plan::Plan); a run instance is that plan
//! plus an identity substitution ([`instance::Instance`]), and every accessor
//! resolves against runner dialog state — never against document text. Nothing
//! is re-parsed per call.
//!
//! **Dumb, with teeth.** The interpreter holds no callflow knowledge: it binds
//! endpoints, sequences the flow, emits what `msg` states, gates an `expect`
//! structurally, answers `background` traffic without moving the cursor,
//! records every datagram verbatim, settles, and evaluates `postconditions`.
//! A corner case is document data or it does not exist. It never reads
//! `calls[].cause`, `relay18x`, `case.annotations`, `case.requires` or
//! `observed`.
//!
//! **Nothing is swallowed.** Every plan-compilation refusal, expect failure,
//! settle failure and recording failure surfaces in the run bundle's
//! `verdict.json` with the step, the leg, what arrived and what was gated. A
//! run that did not fully settle is a failure; there is no soft mode.
//!
//! **The seam is one call.** [`replay`] owns the whole run sequence — compile,
//! arm the bundle writer, drive the executor, commit the bundle — and a caller
//! supplies only lane knowledge: a [`RunConfig`], a [`Lane`] of bound agents
//! with a [`UriComposer`], a [`Sut`], and the bundle directory. [`plan`] stays
//! public beside it for callers that compile a document themselves — lane
//! derivation reads the plan before the run, and a refusal reads [`PlanError`].
//!
//! **The wire contracts are not this crate's.** `RunConfig`, `RunVerdict`,
//! `RunTiming`, `RecordedMessage` and the identity binding are
//! [`pivot_schema::bundle`]'s, so one crate owns every wire type and one
//! binary emits every schema. They are re-exported below because [`replay`] is
//! the front door and a caller should need one import to use it.
//!
//! Module map (one concern per file; interior modules are `pub(crate)` — the
//! crate's surface is [`replay`] plus the types it hands out):
//! - `replay` — the front door: one run, document to bundle.
//! - [`plan`] — document → immutable plan, and every refusal it can state.
//! - `program` — the ordering structure: per-leg item sequences and blocks.
//! - `instance` — plan + bindings: one run.
//! - `state` — runner dialog state and per-step outcomes.
//! - `resolve` — the `${…}` accessor grammar resolved against that state.
//! - `gate` — what an `expect` gates on, and what it does with a datagram
//!   that does not match.
//! - `claim` — how a UAS claims its inbound INVITE.
//! - `cursor` — the scheduler: readiness, `after`, dwell, `alt` commit,
//!   `optional` release, `unordered` completion.
//! - `early` — the early dialogs a forking callee answers under (§6.1).
//! - `deviation` — the reproduced non-compliance an emission must carry.
//! - `preserve` — holding a preserved emission to the block the document
//!   stores (`verbatim-emission`, `raw-order`).
//! - `background` — traffic answered outside the flow, and its settle-time
//!   counters.
//! - `checks` — the one check vocabulary, evaluated over a matched message.
//! - `scope` — lane scoping: what a classified check costs on THIS run.
//! - `must_fail` — the verdict inversion a negative document earns (§11.2).
//! - `progress` — whether a recorded failure leaves the run able to go on.
//! - `close` — what a scripted leg owes when its script ends before its call.
//! - `settle` — the settle contract and the postcondition evaluation.
//! - `recording` — MAKING the verbatim per-leg recording.
//! - `retransmit` — the `retransmits` count: the ladder a `send` emits and
//!   the repeats an `expect` counts (§6.9).
//! - `bundle` — WRITING the run bundle, on the way out of a run.
//! - `render` — a `send` step's message, composed for the wire.
//! - `stack` — the per-leg UA: tier-1 regeneration and the automatics.
//! - `exec` — the run loop that drives a plan over the scenario harness.
//!
//! The §17.2 receive view is NOT one of them: it lives once for the whole tree
//! in `scenario_harness::absorption`, and the interpreter drives it in `exec`
//! so a repeat is recorded before it is dropped.

pub(crate) mod background;
pub(crate) mod bundle;
pub(crate) mod checks;
pub(crate) mod claim;
pub(crate) mod close;
pub(crate) mod cursor;
pub(crate) mod deviation;
pub(crate) mod early;
pub(crate) mod exec;
pub(crate) mod gate;
pub(crate) mod instance;
pub(crate) mod media;
pub(crate) mod must_fail;
pub mod plan;
pub(crate) mod preserve;
pub(crate) mod program;
pub(crate) mod progress;
pub(crate) mod recording;
pub(crate) mod render;
mod replay;
pub(crate) mod resolve;
pub(crate) mod retransmit;
pub(crate) mod scope;
pub(crate) mod settle;
pub(crate) mod stack;
pub(crate) mod state;

pub use exec::{Lane, Outcome};
pub use media::Booking;
pub use plan::{Plan, PlanError};
pub use recording::Recording;
pub use render::UriComposer;
pub use replay::{replay, ReplayError};
pub use settle::Sut;

/// The run bundle's wire contracts, owned by [`pivot_schema::bundle`].
pub use pivot_schema::bundle::{
    Abandoned, BindingError, CheckDisposition, ClockMode, CloseAct, CloseOwed, DeclaredNote, Dir,
    Failure, IdentityBindings, Informative, RecordedMessage, RetransmitNote, RunConfig, RunTiming,
    RunVerdict, TimingNote, VerdictStatus, ViolationNote,
};
