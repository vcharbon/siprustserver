//! The cluster post-condition a failover cell ends on, for an incident whose
//! CDR count is a range (ADR-0055 §5).

use std::ops::RangeInclusive;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, FailoverHarness, ReplicatedB2buaSut,
};
use scenario_harness::agent::Agent;

/// Settled, a count of CDRs within `cdrs` across the cluster, no trace and
/// clean per-call memory on both nodes.
pub(crate) async fn settle_to_cdrs(
    fh: &FailoverHarness,
    peers: &[&Agent],
    nodes: &[&ReplicatedB2buaSut; 2],
    call_ref: &str,
    cdrs: RangeInclusive<usize>,
) {
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(call_ref).await
                && !nodes[1].holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the failed-over call within the settle budget");
    fh.linger_peers(peers, Duration::from_secs(3)).await;
    let written = total_cdrs_for(&nodes[..], call_ref);
    assert!(
        cdrs.contains(&written),
        "{written} CDR(s) for the failed-over call across the cluster, {cdrs:?} expected",
    );
    assert_call_fully_released(&nodes[..], call_ref).await;
}
