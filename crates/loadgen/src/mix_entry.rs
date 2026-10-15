//! One entry of the run's weighted mix: a shape's load body, its pick weight,
//! its attached Test case and the load attributes its descriptor declares.

use std::sync::Arc;

use e2e_model::{LegSpec, Scenario, ScenarioInputs, ShapeDescriptor, ShapeRegistry};
use scenario_harness::actor::ActorScenario;

use crate::case::LoadCase;

/// A load shape's per-call body: the declarative actor [`Scenario`] the runner
/// drives. `run_one` invokes it at the single body-invocation site.
#[derive(Clone)]
pub(crate) enum LoadBody {
    Actor(Scenario),
}

/// One scenario mix entry: the shape's **report/metrics id** + load attributes
/// (from its [`ShapeDescriptor`] — the shape's ONE declaration), the load body,
/// its pick weight, and an optional attached Test case
/// (`--scenario name=w,case=<path.json>` / the global `--case`) whose binding
/// pool drives per-call identities and dwells.
///
/// Build from the unified registry ([`MixEntry::by_id`] /
/// [`MixEntry::default_mix`] / [`MixEntry::abnormal_end_mix`]); plain
/// `(scenario, weight)` tuples still convert for hand-rolled test bodies
/// (attributes default off, id from the body).
#[derive(Clone)]
pub struct MixEntry {
    /// The report/metrics id — the DESCRIPTOR's id (may differ from the body's
    /// intrinsic `id()` when one body serves several shapes, e.g. `basic_call_em`).
    pub id: crate::scenarios::ScenarioId,
    /// The load body — a declarative actor [`Scenario`] the runner drives.
    /// `run_one` invokes it at the single body-invocation site; everything after
    /// (teardown, classification, sampling) is shared.
    pub(crate) body: LoadBody,
    pub weight: f64,
    pub case: Option<Arc<LoadCase>>,
    /// The callee legs bound for this shape's calls
    /// ([`ShapeDescriptor::callee_legs`]): each leg's agent-name **role** plus
    /// the R-URI user-part **prefixes** that demux it on the shared UAS
    /// socket, in the load-bearing bind order.
    pub legs: &'static [LegSpec],
    /// Stamp `Resource-Priority: esnet.0` (the SUT force-admits under overload).
    pub emergency: bool,
    /// **Deferred-by-design auth adapter** (see
    /// `scenario_harness::realcall::auth`). `None` (the default) = no RFC 3261
    /// §22.2 retry; a `401`/`407` classifies as `status_401`/`status_407`. Set it
    /// (via [`MixEntry::with_challenge_responder`]) to point the fleet at a
    /// challenging registrar/proxy once digest lands — no CLI flag mints one yet.
    pub challenge_responder: Option<Arc<dyn scenario_harness::realcall::ChallengeResponder>>,
}

impl From<(Arc<dyn ActorScenario>, f64)> for MixEntry {
    fn from((scenario, weight): (Arc<dyn ActorScenario>, f64)) -> Self {
        MixEntry {
            id: scenario.id(),
            body: LoadBody::Actor(scenario),
            weight,
            case: None,
            legs: LegSpec::historic(false, false),
            emergency: false,
            challenge_responder: None,
        }
    }
}

impl MixEntry {
    /// Build a mix entry from a shape's descriptor (minting its load body from
    /// the per-run `inputs`). `None` when the shape has no load body
    /// (functional-only).
    pub fn from_shape(
        shape: &ShapeDescriptor,
        inputs: &ScenarioInputs,
        weight: f64,
    ) -> Option<Self> {
        // The declarative actor scenario (functional-only shapes have none → None).
        let body = LoadBody::Actor(shape.load_scenario(inputs)?);
        Some(MixEntry {
            id: shape.id,
            body,
            weight,
            case: None,
            legs: shape.callee_legs(),
            emergency: shape.emergency,
            challenge_responder: None,
        })
    }

    /// Resolve a shape by id in the registry (the CLI's `--scenario name=weight`).
    /// `None` for an unknown id or a functional-only shape.
    pub fn by_id(
        registry: &ShapeRegistry,
        id: &str,
        inputs: &ScenarioInputs,
        weight: f64,
    ) -> Option<Self> {
        registry.get(id).and_then(|d| Self::from_shape(d, inputs, weight))
    }

    /// The shipped DEFAULT mix (every shape with a `default_weight`,
    /// basic-heavy like real traffic).
    pub fn default_mix(registry: &ShapeRegistry, inputs: &ScenarioInputs) -> Vec<Self> {
        registry
            .default_mix()
            .into_iter()
            .filter_map(|d| Self::from_shape(d, inputs, d.default_weight.unwrap_or(1.0)))
            .collect()
    }

    /// The abnormal-end mix (one shape per post-call-cleanup teardown path, each
    /// ending on its own accepted branch), for the no-leak cleanup-coverage
    /// test.
    pub fn abnormal_end_mix(registry: &ShapeRegistry, inputs: &ScenarioInputs) -> Vec<Self> {
        registry
            .abnormal_end_mix()
            .into_iter()
            .filter_map(|d| Self::from_shape(d, inputs, d.abnormal_end_weight.unwrap_or(1.0)))
            .collect()
    }

    /// Attach a Test case (binding pool → per-call identities + dwells).
    pub fn with_case(mut self, case: Option<Arc<LoadCase>>) -> Self {
        self.case = case;
        self
    }

    /// Attach the deferred-by-design auth adapter (see the field docs). The load
    /// driver folds it into every call's `CallEnv`; the default is `None`.
    pub fn with_challenge_responder(
        mut self,
        responder: Option<Arc<dyn scenario_harness::realcall::ChallengeResponder>>,
    ) -> Self {
        self.challenge_responder = responder;
        self
    }
}

#[cfg(test)]
mod tests;
