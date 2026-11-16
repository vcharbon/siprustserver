//! Any [`ActorScenario`], fuzzed.
//!
//! The wrapped scenario builds its call as it always does; the fuzzer then,
//! where the caller's [`Policy`] states the accepted endings, holds plain
//! answering callees ([`crate::hold_callees`]) and may inject a caller CANCEL
//! ([`crate::inject_cancel`]), and always applies the common perturbation
//! ([`crate::perturb`]). A shape whose policy states no endings keeps its own
//! and is only jittered and loaded with payloads.

use std::sync::Arc;

use e2e_model::{ShapeDescriptor, ShapeRegistry};
use scenario_harness::actor::{ActorCall, ActorScenario, Branch, Expect};
use scenario_harness::realcall::{CallEnv, ScenarioId};
use scenario_harness::StepError;

use crate::{draw_trains, hold_callees, inject_cancel, offer_100rel, perturb, seal};
use crate::{FuzzConfig, Rng};

/// What the fuzzer may change on one wrapped scenario.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// The endings every legal mutation of the scenario may reach: callees
    /// may be held and a CANCEL injected, the accepted endings replaced by
    /// these. `None`: the call keeps its goals and endings.
    pub endings: Option<&'static [Branch]>,
    /// The scenario's callee legs are offered 100rel, so a held callee may
    /// ring reliably.
    pub callee_reliable: bool,
    /// The roles of the plain callees the fuzzer may hold.
    pub callee_roles: &'static [&'static str],
}

/// `inner` under a fuzz [`Policy`], known by `id`.
pub struct FuzzWrap {
    pub id: &'static str,
    pub inner: Arc<dyn ActorScenario>,
    pub policy: Policy,
    pub config: FuzzConfig,
}

impl FuzzWrap {
    /// Hold callees, maybe inject a CANCEL, and state the policy's endings.
    fn mutate(
        &self,
        rng: &mut Rng,
        call: &mut ActorCall,
        endings: &'static [Branch],
        notes: &mut Vec<String>,
    ) {
        let roles = self.policy.callee_roles;
        if hold_callees(rng, call, roles, self.policy.callee_reliable, notes) {
            offer_100rel(call);
        }
        inject_cancel(rng, call, notes);
        call.expect = Expect::of(endings);
    }
}

impl ActorScenario for FuzzWrap {
    fn id(&self) -> ScenarioId {
        self.id
    }

    /// The inner call, mutated and perturbed from the call's seed, sealed with
    /// a `Wrap | <inner id> <notes>` draw.
    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError> {
        let seed = self.config.seed_of(&env.token);
        let mut rng = Rng::new(seed);
        let mut call = self.inner.build(env)?;
        let mut notes = vec![self.inner.id().to_string()];
        if let Some(endings) = self.policy.endings {
            self.mutate(&mut rng, &mut call, endings, &mut notes);
        }
        perturb(&mut rng, &mut call, &self.config, &mut notes);
        if self.config.train && self.policy.endings.is_some() {
            let max = self.config.train_max;
            draw_trains(seed, &mut call, max, self.policy.callee_reliable, &mut notes);
        }
        let desc = format!("Wrap | {}", notes.join(" "));
        seal(&mut call, seed, &env.token, desc, self.config.log_draws);
        Ok(call)
    }
}

/// The id of `inner`'s fuzz wrapper.
pub fn wrapped_id(inner: &str) -> String {
    format!("fz_{inner}")
}

/// Every shape of `catalog` that `policy` names and that has a load body, as
/// a [`wrapped_id`] descriptor with the shape's legs and needs and no weight
/// of its own.
pub fn wrapped_shapes(
    catalog: &ShapeRegistry,
    config: FuzzConfig,
    policy: impl Fn(&str) -> Option<Policy>,
) -> Vec<ShapeDescriptor> {
    catalog
        .iter()
        .filter_map(|d| {
            let policy = policy(d.id)?;
            let factory = d.load.clone()?;
            let id: &'static str = Box::leak(wrapped_id(d.id).into_boxed_str());
            let mut w = d.clone();
            w.id = id;
            w.default_weight = None;
            w.abnormal_end_weight = None;
            Some(w.load_actor_with(move |inputs| {
                Arc::new(FuzzWrap { id, inner: factory(inputs), policy, config })
                    as Arc<dyn ActorScenario>
            }))
        })
        .collect()
}
