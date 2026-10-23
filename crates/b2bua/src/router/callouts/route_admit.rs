//! The route-admit fold the failover and release callouts share: one
//! replacement of the call's admission set by a route's limiter entries.

use call::AdmitReport;

use crate::decision::RouteDecision;
use crate::limiter::{AdmitOutcome, LimiterHeld};
use crate::metrics::AdmitSite;
use crate::router::RouterCtx;

/// Replace the call's set on the cluster limiter with a route's
/// `call_limiter` entries — the ONE fold shared by the failover and release
/// callouts, so the two can never drift from each other. One
/// `admit(key, change, held, entries, release_on_refusal = true)`, checked
/// net of the set the call holds (`held`, the call's held set, which it
/// carries): a refusal releases that set in the same step. Nothing is sent for
/// a route stating no limiter when the limiter can hold nothing for the key
/// (`may_hold` false): the report states an unsent admit of nothing. Held alone
/// does not tell: an admit whose answer was lost may have landed, and the
/// empty replacement is what drops its set. The worker answers just
/// past the limiter's admit budget, `unavailable` when the limiter did not,
/// so the consult's answer deadline lies past the whole chain. `Ok(report)`: the
/// admit the fold carries — admitted, lost, superseded, or refused by a release
/// fence (the call ended; counted as
/// `b2bua_limiter_admit_released_total{site="fold"}`); `Err(report)`: refused
/// on a cap — the caller owns the treatment, the call holds nothing and owes
/// its release.
pub(super) async fn admit_route_limiters(
    ctx: &RouterCtx,
    key: &str,
    held: &LimiterHeld,
    may_hold: bool,
    change: u64,
    route: &RouteDecision,
) -> Result<AdmitReport, AdmitReport> {
    if route.call_limiter.is_empty() && !may_hold {
        let outcome = AdmitOutcome::NotSent;
        return Ok(AdmitReport { key: key.to_string(), change, entries: Vec::new(), outcome });
    }
    let entries = route.call_limiter.clone();
    let report = ctx.limiter.admit(AdmitSite::Fold, key, change, held, entries, true).await;
    match &report.outcome {
        AdmitOutcome::Rejected { .. } => Err(report),
        _ => Ok(report),
    }
}
