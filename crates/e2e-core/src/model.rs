//! The authored JSON surface (ADR-0018 Phase D) — MOVED to the dependency-light
//! `e2e-model` crate (the axis data model shared with the load generator) and
//! re-exported here verbatim so every consumer path (`e2e_core::model::…`,
//! including the `xtask e2e-schema` emission via [`schemas`]) is unchanged.
//!
//! [`validate_case`] is generic over [`e2e_model::ShapeSpec`]; `e2e-core`
//! bridges `dyn CallflowShape` onto it (see [`crate::shape`]), so passing the
//! compiled shape registry works exactly as before.

pub use e2e_model::model::*;
