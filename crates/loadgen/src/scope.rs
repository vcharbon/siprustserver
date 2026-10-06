//! Re-export shim. The early-exit teardown registry ([`CallScope`]) lives in
//! `scenario_harness::realcall`, shared with the in-process functional leak gate;
//! this module path keeps `crate::scope::CallScope` imports resolving.

pub use scenario_harness::realcall::CallScope;
