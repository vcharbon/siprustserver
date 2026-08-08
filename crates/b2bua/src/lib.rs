//! `b2bua` — the B2BUA core: per-call dispatch, the rule engine, and the call
//! router.
//!
//! Layers, bottom-up:
//! - [`store`] — the in-memory call map + per-call serialization over a
//!   replication-aware [`store::CallStore`] seam (HA drops in later, no changes
//!   to rules/dispatch).
//! - [`dispatch`] — the per-call FIFO: a bounded queue + worker task per call,
//!   capped globally (ADR-0004/0005).
//! - [`timers`] — one `DelayQueue` driver firing [`event::CallEvent::Timer`].
//! - [`decision`] — the call-decision adapter seam + a scripted test impl.
//! - [`rules`] — first-match, layer-ranked rule engine + invariant enforcement.
//! - [`router`] — consumes the transaction-layer event stream, resolves the
//!   `callRef`, runs the handler, interprets the typed [`effects`].
//! - [`lifecycle`] — the aggregated lifecycle-log vocabulary (ADR-0026).
//! - [`trace`] — the per-call trace gate, root spans and guarded emission
//!   vocabulary (ADR-0026).
//! - [`b2bua_core`] — wires it all together.
//!
//! Builds on the `call` data model (ADR-0010).

pub mod b2bua_core;
pub mod cdr;
pub mod config;
pub mod decision;
pub mod dispatch;
pub mod drain;
pub mod effects;
pub mod event;
pub mod initial_invite;
pub mod lifecycle;
pub mod limiter;
pub mod limiter_http;
pub mod metrics;
pub mod obligations;
pub mod overload;
pub mod peer_failures;
pub mod reaper;
pub mod repl;
pub mod router;
pub mod rules;
pub mod stack_identity;
pub mod store;
pub mod target_admission;
pub mod tier1_brake;
pub mod timers;
pub mod trace;

pub use b2bua_core::{B2buaCore, B2buaDeps, ReplicationSetup};
pub use router::AdaptationHttpPort;

pub use config::B2buaConfig;
pub use effects::{HandlerEffects, HandlerResult};
pub use event::CallEvent;
pub use metrics::{B2buaMetrics, BufferedSendCounters, LiveGauge, UdpTransportMetrics};
// The callflow-service authoring macros live in the public Rule SDK (ADR-0016
// slice 6); re-export them so in-tree services keep using `b2bua::define_service!`
// / `b2bua::sm_rule!`. (`$crate` inside the macro resolves to `b2bua_sdk`, where
// the SDK vocabulary the expansion references lives.)
pub use b2bua_sdk::{define_service, sm_rule};
