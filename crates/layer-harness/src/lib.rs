//! layer-harness — the test-only contract + recording foundation shared by
//! every layer (ADR-0004).
//!
//! A layer's contracts (`paranoidInputs` / `scopedAudit` and their kin) are
//! **decorator structs that implement the layer's own trait** around the real
//! implementation, and they all stand on the same shared machinery:
//!
//! | Piece | Here |
//! |---|---|
//! | three-tier severity | [`RunContext`], [`Severity`] |
//! | per-scenario event order | [`EventSequencer`] |
//! | typed recording channels + projectors | [`Recorder`], [`Channel`] |
//! | recording helpers | [`recording`] |
//! | anomaly ledger | [`RecordedAnomaly`] |
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
