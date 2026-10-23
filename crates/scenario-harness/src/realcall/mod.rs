//! Real-call scenario support — the per-call environment ([`CallEnv`]), recorder
//! ([`CallCtx`]), teardown scope ([`CallScope`]) and deferred-auth adapter
//! ([`auth`]) shared by the load generator (`crates/loadgen`, which counts each
//! [`ScenarioVerdict`]) and the in-process functional leak gate.
//!
//! The scenario *bodies* themselves are ACTOR-declared — per-endpoint reactive
//! actors the runner joins — and live in [`crate::actor::scenarios`]; this module
//! provides the environment they build against plus the two functional-gate
//! runners ([`run_actor_collecting`] / [`run_actor_asserting`]).

use crate::actor::ScenarioVerdict;

pub mod auth;
mod ctx;
mod env;
mod scope;

pub use auth::{Challenge, ChallengeResponder};
pub use ctx::CallCtx;
pub use env::{CallEnv, CoreIdentity, CorrelationStamp, InvitePlan};
pub use scope::CallScope;

/// Stable scenario name — the Prometheus `scenario` label and report dir name.
pub type ScenarioId = &'static str;

/// Run an [`ActorScenario`](crate::actor::ActorScenario) once and **return**
/// its [`ScenarioVerdict`] — the in-process functional leak gate that OBSERVES
/// the outcome. The runner owns the per-actor teardown scopes (they survive a
/// body panic — see [`run_call_with`](crate::actor::run_call_with)), so no
/// outer `CallScope` is needed: after this returns, every leg the call opened
/// on the SUT has been released (its own BYE or the accepted ending's final on
/// a clean call, best-effort CANCEL/BYE otherwise), and the caller can assert
/// on the SUT (`active_calls() == 0`, `assert_fully_reaped()`) with no leaked
/// dialog left behind.
pub async fn run_actor_collecting(
    scenario: &dyn crate::actor::ActorScenario,
    env: &CallEnv<'_>,
) -> ScenarioVerdict {
    let ctx = CallCtx::new();
    crate::actor::run_actor_scenario(scenario, env, &ctx).await
}

/// Run an [`ActorScenario`](crate::actor::ActorScenario) as a **strict**
/// in-process functional leak gate: drive it once (teardown included), `panic!`
/// if it failed, and return the accepted branch it ended on. The caller then
/// asserts on the SUT (`active_calls() == 0`, `assert_fully_reaped()`).
pub async fn run_actor_asserting(
    scenario: &dyn crate::actor::ActorScenario,
    env: &CallEnv<'_>,
) -> &'static str {
    match run_actor_collecting(scenario, env).await {
        ScenarioVerdict::Success { branch } => branch,
        ScenarioVerdict::Failure { at, cause } => {
            panic!("actor scenario `{}` failed at {at}: {cause:?}", scenario.id())
        }
    }
}
