//! The layout-owned **egress rewrite** (ADR-0018) — MOVED to the
//! dependency-light `e2e-model` crate (the axis data model shared with the
//! load generator) and re-exported here verbatim so every consumer path
//! (`e2e_core::egress::…`) is unchanged. Each [`crate::infra::InfraShape`]
//! still declares its [`EgressPolicy`]; the runtime seams
//! ([`crate::infra::InfraRuntime::callee`] /
//! [`crate::infra::InfraRuntime::outgoing_invite`]) stay in `e2e-core`.

pub use e2e_model::egress::*;
