//! Rule selection + execution — port of `Matcher.ts` (`pickRanked`) +
//! `RuleExecutor.ts`. First handler returning `Some` wins; its actions run
//! through the [`ActionExecutor`], then termination is finalized + invariants
//! enforced. No candidate → the default handler.

use std::collections::HashSet;

use crate::effects::HandlerResult;

use super::actions::ActionExecutor;
use super::invariants;
use super::model::{RuleContext, RuleDefinition};

/// Filter rules by columns + filter predicate, drop overridden rules, and sort
/// by layer (desc) then registration order (asc, stable).
pub fn pick_ranked<'a>(rules: &'a [RuleDefinition], ctx: &RuleContext) -> Vec<&'a RuleDefinition> {
    let mut candidates: Vec<(usize, &RuleDefinition)> = rules
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.matcher.accepts_columns(ctx) && r.matcher.filter.is_none_or(|f| f(ctx))
        })
        .collect();

    let overridden: HashSet<&str> = candidates
        .iter()
        .flat_map(|(_, r)| r.overrides.iter().copied())
        .collect();
    candidates.retain(|(_, r)| !overridden.contains(r.id));

    candidates.sort_by(|a, b| b.1.layer.cmp(&a.1.layer).then(a.0.cmp(&b.0)));
    candidates.into_iter().map(|(_, r)| r).collect()
}

/// Run the rule chain for `ctx`. The first matching rule that returns `Some`
/// handles the event; otherwise `default` is invoked.
pub fn execute_rules(
    rules: &[RuleDefinition],
    ctx: &RuleContext,
    exec: &ActionExecutor,
    default: fn(&RuleContext) -> HandlerResult,
) -> HandlerResult {
    for rule in pick_ranked(rules, ctx) {
        if let Some(outcome) = (rule.handle)(ctx) {
            let before = ctx.call.clone();
            let result = exec.execute(&outcome.actions, ctx);
            let result = invariants::finalize(result);
            return invariants::enforce(&before, result);
        }
    }
    default(ctx)
}
