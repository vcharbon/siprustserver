//! The **check engine** (ADR-0019, Phase E) — MOVED to the dependency-light
//! `e2e-model` crate (the axis data model shared with the load generator) and
//! re-exported here verbatim so every consumer path (`e2e_core::checks::…`)
//! is unchanged.

pub use e2e_model::checks::*;
