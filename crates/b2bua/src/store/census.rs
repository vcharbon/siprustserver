//! The census of a snapshot of the live calls: how many rest at each
//! state-machine cursor (ADR-0016), and the sums of the per-call Vecs whose
//! growth localises a leak the map lengths cannot see.

use std::collections::BTreeMap;
use std::sync::Arc;

use call::Call;

use crate::metrics::B2buaMetrics;

/// Publish the census of `calls` to the `b2bua_sm_cursors` gauge and the
/// per-call Vec gauges. The Vec whose sum over `store_calls` climbs while every
/// map length is flat is the leak (a held dialog's per-event tail never pruned
/// till terminal).
pub(super) fn publish(metrics: &B2buaMetrics, calls: &[Arc<Call>]) {
    let mut census: BTreeMap<(String, String), u64> = BTreeMap::new();
    let (mut cdr, mut pend, mut pend_max, mut dialogs, mut rset, mut timers, mut tagmap, mut blegs) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for call in calls {
        for (machine, state) in &call.sm_cursors {
            *census
                .entry((machine.as_str().to_string(), state.as_str().to_string()))
                .or_insert(0) += 1;
        }
        cdr += call.cdr_events.len() as u64;
        timers += call.timers.len() as u64;
        tagmap += call.a_leg.dialogs.iter().map(|d| d.fronts.len() as u64).sum::<u64>();
        blegs += call.b_legs.len() as u64;
        let mut call_pend = 0u64;
        for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
            dialogs += leg.dialogs.len() as u64;
            for d in &leg.dialogs {
                rset += d.route_set.len() as u64;
                call_pend += d.requests.len() as u64;
            }
        }
        pend += call_pend;
        pend_max = pend_max.max(call_pend);
    }
    metrics.set_sm_cursor_census(census);
    metrics.set_call_census(cdr, pend, pend_max, dialogs, rset, timers, tagmap, blegs);
}
