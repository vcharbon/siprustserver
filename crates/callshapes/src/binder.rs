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
}
