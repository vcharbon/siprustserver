//! The **routing seam** — how an abstract "the SUT must route/behave like X"
//! slot becomes concrete INVITE input (spec §3 of the callshapes program).
//!
//! A shape never spells wire syntax for routing: its stages declare a
//! [`RouteIntent`] and the per-platform [`RouteBinder`] realizes it. Upstream
//! tests use [`EgressBinder`] (the historic `CallEnv::invite_plan` seam:
//! `EgressPolicy::Transparent` route-by-config or the `X-Api-Call`
//! pin/failover plan). A downstream platform binds the same intents its own
//! way — e.g. newkahsip maps a `FailoverOnReject` intent to a dialed number
//! whose BL scenario is a reroute, by building an
//! [`InvitePlan`](scenario_harness::realcall::InvitePlan) with its own R-URI
//! user (and any extra headers) instead.

use scenario_harness::realcall::{CallEnv, InvitePlan};

/// What a stage needs from the SUT's routing for its initial INVITE — the
/// abstract behavior slot. Targets are the shape's declared callee ROLES
/// (`"bob"`, `"bob2"`, …), never addresses: the binder owns how a role becomes
/// a wire destination.
#[derive(Debug, Clone, Copy)]
pub enum RouteIntent<'a> {
    /// Deliver the call to a single target role.
    Direct { target: &'static str },
    /// Reject-triggered failover: the SUT tries `targets` in order, advancing
    /// on a reject final from the current one (the reroute family — including
    /// reroute-no-18x, where the first target rejects without any
    /// provisional).
    FailoverOnReject { targets: &'a [&'static str] },
}

impl RouteIntent<'_> {
    /// The intent's target roles in order (first = the initial destination).
    pub fn targets(&self) -> &[&'static str] {
        match self {
            RouteIntent::Direct { target } => std::slice::from_ref(target),
            RouteIntent::FailoverOnReject { targets } => targets,
        }
    }
}

/// Realizes abstract routing intents as concrete INVITE input for ONE
/// platform. Implementations must be cheap per call (called once per
/// establishment stage).
pub trait RouteBinder: Send + Sync {
    /// The initial-INVITE plan realizing `intent` (R-URI, headers, X-Api-Call,
    /// egress rewrite — whatever this platform's SUT routes on).
    fn invite_plan(&self, env: &CallEnv<'_>, intent: RouteIntent<'_>) -> InvitePlan;

    /// The initial-INVITE plan for a **no-answer-triggered** failover
    /// (`Establishment::RerouteOnNoAnswer`, newkahneed-047): the SAME routing
    /// intent as the reject-triggered failover — a downstream platform whose
    /// SUT arms its own ring timer from the dialed plan (e.g. the newkah BC_02
    /// reroute number) needs no override, so the default just delegates to
    /// [`Self::invite_plan`] and IGNORES `no_answer_sec`. A binder whose SUT
    /// timer is client-armed (the upstream [`EgressBinder`]) overrides this to
    /// write the per-route `no_answer_timeout_sec` into its plan.
    fn invite_plan_no_answer(
        &self,
        env: &CallEnv<'_>,
        intent: RouteIntent<'_>,
        no_answer_sec: i64,
    ) -> InvitePlan {
        let _ = no_answer_sec;
        self.invite_plan(env, intent)
    }

    /// The authorization payload a REFER carries so this platform's SUT
    /// accepts the transfer (`None` = the SUT needs none). `refer_key` is the
    /// per-run auth input (`ScenarioInputs::refer_key` upstream).
    fn refer_authorization(&self, env: &CallEnv<'_>, refer_key: &str) -> Option<String> {
        env.refer_authorization(refer_key)
    }
}

/// The upstream binder: intents realize through the env's `EgressPolicy` seam
/// — `Transparent` (the SUT routes by its own config) or `ApiCallPin` (the
/// `X-Api-Call` destination pin / ADR-0017 `routes` failover plan). This is
/// byte-for-byte the historic `env.invite_plan(&[roles…])` behaviour the
/// hand-written scenarios used.
#[derive(Debug, Clone, Copy, Default)]
pub struct EgressBinder;

impl RouteBinder for EgressBinder {
    fn invite_plan(&self, env: &CallEnv<'_>, intent: RouteIntent<'_>) -> InvitePlan {
        env.invite_plan(intent.targets())
    }

    /// Upstream, the SUT's no-answer ring timer is CLIENT-armed: the pinned
    /// layout's `routes` plan carries `no_answer_timeout_sec` per hop
    /// (`CallEnv::invite_plan_no_answer` → `ApiCall::routes_with_no_answer`),
    /// which the decision engine folds into the route so `apply_route` arms
    /// `TimerType::NoAnswer` on the dialed b-leg.
    fn invite_plan_no_answer(
        &self,
        env: &CallEnv<'_>,
        intent: RouteIntent<'_>,
        no_answer_sec: i64,
    ) -> InvitePlan {
        env.invite_plan_no_answer(intent.targets(), no_answer_sec)
    }
}
