//! Re-export shim. The layout-owned egress-rewrite model ([`EgressPolicy`],
//! [`CalleeTarget`], [`EgressRewrite`], the typed [`ApiCall`] control header)
//! now lives in `scenario_harness::egress` — the harness substrate BOTH run
//! surfaces depend on — so the shared realcall choreography's `CallEnv` can
//! carry the policy without a crate cycle (this crate depends on
//! scenario-harness for `Invite`/`RunReport`). Everything stays reachable at
//! its historical `e2e_model::egress::*` / `e2e_core::egress::*` paths.

pub use scenario_harness::egress::*;
