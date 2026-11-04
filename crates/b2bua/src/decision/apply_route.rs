//! `apply_route` — a "route" decision applied on the initial INVITE's turn:
//! the route's folds up to its admit, the admit the turn waits on, and either
//! the b-leg dialled or the limiter-reject failover (ADR-0052 §7). Port of
//! `decision/apply/applyRoute.ts`.

use call::{AdmitOutcome, AdmitReport, Call};
use sip_clock::Clock;
use sip_ids::TurnIds;
use sip_message::SipRequest;

use crate::config::B2buaConfig;
use crate::decision::route_fold::{self, Admitting};
use crate::decision::DeadlineDecisionEngine;
use crate::effects::HandlerResult;
use crate::limiter::{LimiterEntry, LimiterWorker};
use crate::metrics::AdmitSite;
use crate::owners::Owners;
use crate::setup_budget::SetupBudget;

use crate::decision::RouteDecision;

pub use super::route_fold::MAX_LIMITER_FAILOVER;

/// Apply a route decision to `call` (which already carries the a-leg), creating
/// the first b-leg + its outbound INVITE under the attempt the route opens (a
/// route admission refuses is an attempt that ends `Refused`; a route whose
/// dial a service defers opens none here). `depth` tracks chained limiter-reject
/// failovers (start at 0); `invite_wire` is the a-leg INVITE's datagram, which a
/// late trace activation backfills from. `now_ms` is the turn's timestamp (every
/// CDR entry and timer deadline below reads it); `clock` measures the failover
/// round trip and the instant the b-leg's INVITE leaves, after the admission
/// the turn waited on (`OutboundInvite::sent_at_ms`). The consult that answered
/// `route` already honoured its trace force-enable. A route `budget` ran out on
/// dials nothing: it ends as refused, the call answered as for an unavailable
/// decision, its record owing the release of an admit that left.
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
    depth: u32,
    budget: SetupBudget,
) -> HandlerResult {
    let mut admitting =
        match route_fold::up_to_admit(call, route, a_invite, config, ids, now_ms, depth) {
            Admitting::Final(result) => return *result,
            Admitting::Admit(admitting) => admitting,
        };
    let stated = admitting.route.call_limiter.clone();
    let refusal = admit_route(&mut admitting.call, stated, limiter, now_ms, budget).await;
    if budget.spent() {
        let (call, mut fx) = route_fold::refused(*admitting);
        let mut result = super::failover_fold::unavailable(call, a_invite, config, ids, now_ms);
        fx.extend(std::mem::take(&mut result.effects));
        result.effects = fx;
        return result;
    }
    if let Some(limiter_id) = refusal {
        let (call, mut fx) = route_fold::refused(*admitting);
        let mut result = Box::pin(super::limiter_failover::limiter_reject_failover(
            call,
            limiter_id,
            a_invite,
            invite_wire,
            decision,
            limiter,
            config,
            owners,
            ids,
            clock,
            now_ms,
            depth,
            budget,
        ))
        .await;
        fx.extend(std::mem::take(&mut result.effects));
        result.effects = fx;
        return result;
    }
    // The INVITE leaves now, after the admission this turn awaited.
    let sent_ms = clock.now_ms();
    route_fold::admitted(*admitting, a_invite, config, owners, ids, now_ms, sent_ms)
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
