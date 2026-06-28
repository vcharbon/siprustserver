//! Re-export shim. The early-exit teardown registry ([`CallScope`]) now lives in
//! `scenario_harness::realcall`, shared with the in-process functional leak gate.
//! Kept as a module path so existing `crate::scope::CallScope` imports resolve.

pub use scenario_harness::realcall::CallScope;
