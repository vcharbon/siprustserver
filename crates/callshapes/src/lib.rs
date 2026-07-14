//! **callshapes** — the composable, cross-platform call-shape catalog
//! (callshapes program, `docs/todos/callshapes-program.md`).
//!
//! A call shape is COMPOSED, not hand-written: a [`ShapePlan`] chains an
//! [`Establishment`] stage, any number of pipeline [`Stage`]s (in-dialog
//! [`Script`]s and dialog-changing [`Transfer`]s — each stage yields the
//! "current dialog" the next stage runs on), and a [`Teardown`] — and compiles
//! to a `scenario_harness::actor::ActorCall` at build time. One shape
//! definition therefore serves every established-dialog context: the same
//! re-INVITE script runs on the initial dialog, the post-reroute dialog, or
//! the post-REFER dialog, depending only on where it sits in the chain.
//!
//! **Routing is abstract.** A stage declares WHAT the SUT must do (deliver to
//! a target, fail over on reject — [`RouteIntent`]); a per-platform
//! [`RouteBinder`] turns that intent into concrete INVITE input. Upstream
//! ships [`EgressBinder`] (the historic `EgressPolicy` / `X-Api-Call` seam);
//! an external platform (e.g. newkahsip's dialed-number plan) implements its
//! own binder without touching any shape.
//!
//! The shipped catalog ([`shapes`]) regenerates the historic hand-written
//! scenarios through this algebra — same ids, same downstream contract
//! (`docs/todos/actor-harness-p1-contract-table.md`), byte-for-byte on the
//! wire — proving the algebra before new establishment/script capabilities
//! land on it (program phases C/D).

pub mod binder;
pub mod plan;
pub mod shapes;

pub use binder::{EgressBinder, RouteBinder, RouteIntent};
pub use plan::{
    ByeFeed, DwellKnob, Establishment, Script, ShapePlan, Stage, Teardown, Transfer,
};
