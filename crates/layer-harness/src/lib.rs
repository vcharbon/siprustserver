//! layer-harness — the test-only contract + recording foundation shared by
//! every migration layer (Rust port of sipjsserver's
//! `src/test-harness/framework`).
//!
//! This crate is the Rust home of the **effect-layer-test** pattern (see the
//! `effect-layer-test` SKILL and ADR-0013 in the source). In the TypeScript
//! source the four contract wrappers (`propertyTest` / `paranoidInputs` /
//! `parity` / `scopedAudit`) are Effect `Layer`s that wrap an implementation
//! `Layer` of the same `Tag`. The Effect→Rust construct map (see
//! `docs/MIGRATION_STRATEGY.md`) renders that as **decorator structs that
//! implement the same trait** — and they all need the same shared machinery:
//!
//! | Source piece | Here |
//! |---|---|
//! | `RunContext` (three-tier severity) | [`RunContext`], [`Severity`] |
//! | `EventSequencer` | [`EventSequencer`] |
//! | `Recorder` + typed `forTag` channels + projectors | [`Recorder`], [`Channel`] |
//! | `recordingHelpers.ts` | [`recording`] |
//! | `RecordedAnomaly` ledger | [`RecordedAnomaly`] |
//!
//! **What lives here vs. in a layer crate.** This crate is SIP-agnostic: it
//! knows nothing about `SipMessage`, sockets, or any specific service. The
//! per-service rules, event unions, and the decorator structs themselves
//! live in each layer crate's `contracts` module (e.g. `sip-net`'s
//! `contracts.rs`). This crate provides only the reusable substrate the
//! decorators stand on.
//!
//! **TEST-ONLY.** Nothing here belongs in a production layer tree. The
//! recording channels and severity tiers exist to make tests loud; a
//! production build composes the bare implementation only.

pub mod anomaly;
pub mod contracts;
pub mod event_sequencer;
pub mod recorder;
pub mod recording;
pub mod run_context;
pub mod scenario;
pub mod time;

pub use anomaly::{RecordedAnomaly, Severity};
pub use contracts::{AuditRule, ParanoidCheck, ParanoidViolation, PropertyRule, PropertyViolation};
pub use event_sequencer::EventSequencer;
pub use recorder::{Channel, Recorder, Stamped};
pub use run_context::RunContext;
pub use scenario::{lane_key, Lane, LaneKey, NetworkTag, RecordedScenario, TransportKind};
