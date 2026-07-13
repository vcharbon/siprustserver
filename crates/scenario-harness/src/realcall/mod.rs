//! Real-call scenario support — the per-call environment ([`CallEnv`]), recorder
//! ([`CallCtx`]), teardown scope ([`CallScope`]) and deferred-auth adapter
//! ([`auth`]) shared by the load generator (`crates/loadgen`, fallible: a
//! deviation is a counted [`StepError`]) and the in-process functional leak gate.
//!
//! The scenario *bodies* themselves are ACTOR-declared — per-endpoint reactive
//! actors the runner joins — and live in [`crate::actor::scenarios`]; this module
//! provides the environment they build against plus the two functional-gate
//! runners ([`run_actor_collecting`] / [`run_actor_asserting`]).

use crate::StepError;

pub mod auth;
mod env;
mod scope;

pub use auth::{Challenge, ChallengeResponder};
pub use env::{CallCtx, CallEnv, CoreIdentity, CorrelationStamp, InvitePlan};
pub use scope::CallScope;

/// Stable scenario name — the Prometheus `scenario` label and report dir name.
pub type ScenarioId = &'static str;

/// Run an [`ActorScenario`](crate::actor::ActorScenario) once and **return**
/// its outcome — the in-process functional leak gate that OBSERVES the result
/// (an `Err` is the expected outcome for a voluntarily-failing body). The runner
/// owns the per-actor teardown scopes (they survive a body panic — see
/// [`run_call_with`](crate::actor::run_call_with)), so no outer `CallScope` is
/// needed: after this returns, every leg the call opened on the SUT has been
/// released (its own BYE on the happy path, best-effort CANCEL/BYE otherwise),
/// and the caller can assert on the SUT (`active_calls() == 0`,
/// `assert_fully_reaped()`) with no leaked dialog left behind.
pub async fn run_actor_collecting(
    scenario: &dyn crate::actor::ActorScenario,
    env: &CallEnv<'_>,
) -> Result<(), StepError> {
    let ctx = CallCtx::new();
    crate::actor::run_actor_scenario(scenario, env, &ctx).await
}

/// Run an [`ActorScenario`](crate::actor::ActorScenario) as a **strict**
/// in-process functional leak gate: drive it once (teardown included) and
/// `panic!` if it did not reach its declared happy outcome — the assert-on-
/// deviation analogue of [`run_actor_collecting`]. The caller then asserts on the
/// SUT (`active_calls() == 0`, `assert_fully_reaped()`).
pub async fn run_actor_asserting(
    scenario: &dyn crate::actor::ActorScenario,
    env: &CallEnv<'_>,
) {
    if let Err(e) = run_actor_collecting(scenario, env).await {
        panic!("actor scenario `{}` failed: {e:?}", scenario.id());
    }
}
