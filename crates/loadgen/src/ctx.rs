//! Re-export shim. The per-call context ([`CallEnv`], [`CallCtx`]) now lives in
//! `scenario_harness::realcall`, shared with the in-process functional leak gate.
//! Kept as a module path so existing `crate::ctx::{CallEnv, CallCtx}` imports
//! resolve unchanged.

pub use scenario_harness::realcall::{CallCtx, CallEnv, CoreIdentity, CorrelationStamp};
