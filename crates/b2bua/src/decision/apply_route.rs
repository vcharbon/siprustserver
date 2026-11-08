//! `apply_route` — a "route" decision applied on the initial INVITE's turn:
//! the route's folds up to its admit, the admit the turn waits on, and either
//! the b-leg dialled or the limiter-reject failover (ADR-0052 §7). Port of
//! `decision/apply/applyRoute.ts`.

use call::{AdmitOutcome, AdmitReport, Call};
use sip_clock::Clock;
use sip_ids::TurnIds;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::decision::failover_fold::Failover;
use crate::decision::route_fold::{self, Admitting, AdmittingRoute};
use crate::decision::DeadlineDecisionEngine;
use crate::effects::{HandlerEffects, HandlerResult};
use crate::limiter::{LimiterEntry, LimiterWorker};
use crate::metrics::AdmitSite;
use crate::owners::Owners;
use crate::setup_budget::SetupBudget;

use crate::decision::RouteDecision;

pub use super::route_fold::MAX_LIMITER_FAILOVER;

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE under the attempt the route opens (a
/// route admission refuses is an attempt that ends `Refused`; a route whose
/// dial a service defers opens none here). A route the limiter refuses raises
/// the limiter-reject failover consult, and the failover route it answers is
/// applied the same way, each one hop deeper; `invite_wire` is the a-leg
/// INVITE's datagram, which a late trace activation backfills from. `now_ms`
/// is the turn's timestamp (every CDR entry and timer deadline below reads
/// it); `clock` measures the failover round trip and the instant the b-leg's
/// INVITE leaves, after the admission the turn waited on
/// (`OutboundInvite::sent_at_ms`). The consult that answered `route` already
/// honoured its trace force-enable. A route `budget` ran out on dials nothing:
/// it ends as refused, the call answered as for an unavailable decision, its
/// record owing the release of an admit that left.
#[allow(clippy::too_many_arguments)]
pub async fn apply_route(
    call: Call,
    route: RouteDecision,
    a_invite: &SipRequest,
    invite_wire: &[u8],
    decision: &DeadlineDecisionEngine,
    limiter: &LimiterWorker,
    config: &B2buaConfig,
    owners: &Owners,
    ids: &mut TurnIds,
    clock: &Clock,
    now_ms: i64,
    budget: SetupBudget,
) -> HandlerResult {
    let admitting = match route_fold::up_to_admit(call, route, a_invite, config, now_ms, 0) {
        Admitting::Final(result) => return *result,
        Admitting::Admit(admitting) => admitting,
    };
    let applied = admit(admitting, a_invite, config, limiter, owners, ids, clock, now_ms, budget);
    let refused = match applied.await {
        Applied::Done(result) => return result,
        Applied::Refused(refused) => refused,
    };
    // Boxed, as the refusal path alone runs it: its future stays out of every
    // initial turn's own.
    let turn = Turn { a_invite, invite_wire, decision, limiter, config, owners, clock, now_ms };
    Box::pin(failovers(refused, turn, ids, budget)).await
}

/// What every route of one initial turn is applied under.
#[derive(Clone, Copy)]
struct Turn<'a> {
    a_invite: &'a SipRequest,
    invite_wire: &'a [u8],
    decision: &'a DeadlineDecisionEngine,
    limiter: &'a LimiterWorker,
    config: &'a B2buaConfig,
    owners: &'a Owners,
    clock: &'a Clock,
    now_ms: i64,
}

/// The limiter-reject failovers chained from the refused route `first`: each
/// refusal's consult, and the failover route it answers applied one hop
/// deeper, until a route is dialled or the call's final is answered. Past
/// `MAX_LIMITER_FAILOVER` hops the consult answers the final, which ends the
/// chain. The effects of every refused route come first, in order.
async fn failovers(
    first: Box<Refused>,
    turn: Turn<'_>,
    ids: &mut TurnIds,
    budget: SetupBudget,
) -> HandlerResult {
    let Turn { a_invite, invite_wire, decision, limiter, config, owners, clock, now_ms } = turn;
    let (mut effects, mut refused) = (HandlerEffects::new(), first);
    loop {
        let Refused { call, limiter_id, effects: route_effects, depth } = *refused;
        effects.extend(route_effects);
        let failover = super::limiter_failover::limiter_reject_failover(
            call,
            limiter_id,
            a_invite,
            invite_wire,
            decision,
            config,
            clock,
            now_ms,
            depth,
            budget,
        )
        .await;
        let (call, route) = match failover {
            Failover::Final(result) => return after(effects, *result),
            Failover::Route { call, route } => (call, route),
        };
        let next = depth + 1;
        let admitting = match route_fold::up_to_admit(*call, *route, a_invite, config, now_ms, next)
        {
            Admitting::Final(result) => return after(effects, *result),
            Admitting::Admit(admitting) => admitting,
        };
        let applied =
            admit(admitting, a_invite, config, limiter, owners, ids, clock, now_ms, budget).await;
        refused = match applied {
            Applied::Done(result) => return after(effects, result),
            Applied::Refused(again) => again,
        };
    }
}

/// `result` with `refused`, the effects of the routes refused before it, in
/// front of its own.
fn after(mut refused: HandlerEffects, mut result: HandlerResult) -> HandlerResult {
    refused.extend(std::mem::take(&mut result.effects));
    result.effects = refused;
    result
}

/// What admitting one route came to.
enum Applied {
    /// The b-leg dialled, or the call's final.
    Done(HandlerResult),
    /// The limiter refused the route.
    Refused(Box<Refused>),
}

/// A route the limiter `limiter_id` refused, `depth` failovers deep: `call`
/// holds the refused attempt, `effects` what the route's folds produced.
struct Refused {
    call: Call,
    limiter_id: String,
    effects: HandlerEffects,
    depth: u32,
}

/// A route folded up to its admit: the admit, then the b-leg dialled or the
/// refusal.
#[allow(clippy::too_many_arguments)]
async fn admit(
    mut admitting: Box<AdmittingRoute>,
    a_invite: &SipRequest,
    config: &B2buaConfig,
    limiter: &LimiterWorker,
    owners: &Owners,
    ids: &mut TurnIds,
    clock: &Clock,
    now_ms: i64,
    budget: SetupBudget,
) -> Applied {
    let stated = admitting.route.call_limiter.clone();
    let refusal = admit_route(&mut admitting.call, stated, limiter, now_ms, budget).await;
    if budget.spent() {
        let (call, fx) = route_fold::refused(*admitting);
        let result = super::failover_fold::unavailable(call, a_invite, config, now_ms);
        return Applied::Done(after(fx, result));
    }
    if let Some(limiter_id) = refusal {
        let depth = admitting.depth;
        let (call, effects) = route_fold::refused(*admitting);
        return Applied::Refused(Box::new(Refused { call, limiter_id, effects, depth }));
    }
    // The INVITE leaves now, after the admission this turn awaited.
    let sent_ms = clock.now_ms();
    Applied::Done(route_fold::admitted(*admitting, a_invite, config, owners, ids, now_ms, sent_ms))
}

/// Admission control: one admit of the route's whole limiter set, as
/// `stated`, keyed by the call and numbered by its next change — all or
/// none, nothing sent for a route stating none or with no `budget` left. The
/// initial route holds nothing yet, so a refusal releases nothing. The worker
/// answers within the admit bound, the budget earlier when it runs out first:
/// that cut reads as an unavailable limiter, the admit it dropped owing its
/// release as a lost one does. [`route_fold::admit_outcome`] applies the answer.
/// The refusing limiter's id when a cap refused the route.
async fn admit_route(
    call: &mut Call,
    stated: Vec<LimiterEntry>,
    limiter: &LimiterWorker,
    now_ms: i64,
    budget: SetupBudget,
) -> Option<String> {
    if stated.is_empty() || budget.spent() {
        return None;
    }
    let (change, held) = call.limiter.number_admit();
    let key = call.limiter.key().to_string();
    let admit = limiter.admit(AdmitSite::Initial, &key, change, &held, stated.clone(), false);
    let report = budget.bound(admit).await.unwrap_or_else(|| AdmitReport {
        key: key.clone(),
        change,
        entries: stated,
        outcome: AdmitOutcome::Unavailable,
    });
    if crate::trace::sampled(call) {
        crate::trace::emit::limiter(
            call,
            now_ms,
            "admit",
            &format!("{:?} -> {:?}", report.entries, report.outcome),
        );
    }
    route_fold::admit_outcome(call, &report)
}
