//! `b2bua` — the B2BUA's shell: per-call dispatch, the call router and every
//! effect a turn of the decision core (`b2bua-core`, ADR-0052) returns.
//!
//! Layers, bottom-up:
//! - [`store`] — the in-memory call map + per-call serialization over a
//!   replication-aware [`store::CallStore`] seam (HA drops in later, no changes
//!   to rules/dispatch).
//! - [`dispatch`] — the per-call FIFO: a queue + worker task per call, capped
//!   globally, each event treated by its dispatch class's row (ADR-0010 X2).
//! - [`timers`] — one `DelayQueue` driver firing [`CallEvent::Timer`].
//! - [`decision`] — the call-decision adapter seam + a scripted test impl.
//! - [`rules`] — the core's first-match, layer-ranked rule engine + invariant
//!   enforcement, re-exported.
//! - [`router`] — consumes the transaction-layer event stream, resolves the
//!   `callRef`, runs the core's turn, interprets the typed [`effects`].
//! - [`admission`] — the ordered ladder of rungs that may refuse a new
//!   INVITE, and the one answer a refusal draws (ADR-0037).
//! - [`lifecycle`] — the aggregated lifecycle-log vocabulary (ADR-0026).
//! - [`trace`] — the per-call trace gate, root spans and guarded emission
//!   vocabulary (ADR-0026).
//! - [`model_invariants`] — the model invariant oracle (ADR-0052 §8).
//! - [`node`] — wires it all together.
//!
//! The core's public modules are re-exported under the paths this crate
//! exposed, so its dependents keep their imports.
//!
//! Builds on the `call` data model (ADR-0010).
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::string_slice,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

pub(crate) mod abort_on_drop;
pub mod admission;
pub mod capacity;
pub mod cdr;
pub mod decision;
pub mod dispatch;
pub mod drain;
pub mod ingress_brake;
pub mod initial_consult;
pub mod lifecycle;
pub mod limiter;
pub mod loops;
pub mod metrics;
pub mod model_invariants;
pub mod new_calls;
pub mod node;
pub mod overload;
pub mod peer_failures;
pub mod poison;
pub mod reaper;
pub mod release_all;
pub mod repl;
pub mod router;
pub mod store;
pub mod supervise;
pub(crate) mod sweep;
pub mod timers;
pub mod trace;
pub mod wire_faults;

#[cfg(test)]
pub(crate) use ::b2bua_core::test_call;
pub use ::b2bua_core::{
    answer_deadline, attempts, config, decision_log, destination_allowlist, effects,
    failure_terminate, failure_unavailable, initial_invite, landing, message_ring, obligations,
    reaper_verdict, ring_deadline, rule_chain, rules, stack_identity, timer_arm, turn_notes,
    turn_scope, wire_fault_set,
};

pub use node::{B2buaCore, B2buaDeps, ReplicationSetup};
pub use router::AdaptationHttpPort;

pub use b2bua_sdk::event::CallEvent;
pub use config::B2buaConfig;
pub use effects::{HandlerEffects, HandlerResult};
pub use metrics::{B2buaMetrics, LiveGauge, RemovalClass, UdpTransportMetrics};
/// The process's identifier generator: the counter stream outside any call's
/// turn, and the key every turn draws its own identifiers under.
pub use sip_txn::IdGen;
pub use wire_faults::{WireFaultPoint, WireFaults};
// The callflow-service authoring macros live in the public Rule SDK (ADR-0016);
// re-export them so in-tree services keep using `b2bua::define_service!` /
// `b2bua::sm_rule!`. (`$crate` inside the macro resolves to `b2bua_sdk`, where
// the SDK vocabulary the expansion references lives.)
pub use b2bua_sdk::{define_service, sm_rule};
