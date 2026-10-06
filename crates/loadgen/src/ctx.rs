//! Re-export shim. The per-call context ([`CallEnv`], [`CallCtx`]) lives in
//! `scenario_harness::realcall`, shared with the in-process functional leak gate;
//! this module path keeps `crate::ctx::{CallEnv, CallCtx}` imports resolving.

pub use scenario_harness::realcall::{
    CallCtx, CallEnv, Challenge, ChallengeResponder, CoreIdentity, CorrelationStamp,
};
