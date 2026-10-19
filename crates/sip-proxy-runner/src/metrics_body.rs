//! The proxy's `/metrics` body and its catalogue: the data path, the
//! proxy-self gate, the process-wide observability counters, then the
//! allocator's exposition.

use metric_catalogue::Catalogue;
use sip_proxy::observability::catalogue::{PROXY, SELF_GATE};
use sip_proxy::observability::metrics::self_gate_text;
use sip_proxy::self_gate::ProxySelfGateMetrics;
use sip_proxy::ProxyMetrics;

/// Every family of [`body`] with the allocator's exposition, in order.
pub const CATALOGUE: Catalogue = Catalogue {
    binary: "sip-proxy-runner",
    sections: &[
        PROXY,
        SELF_GATE,
        sip_proxy::supervise::FAMILIES,
        observe::counters::FAMILIES,
        jemalloc_stats::catalogue::FAMILIES,
    ],
};

/// The `/metrics` body: the data path, the self gate (`None` for the
/// always-admit gate), the restarted tasks, the dropped log lines and
/// trace-admission denials (ADR-0026), then `allocator`.
pub fn body(
    metrics: &ProxyMetrics,
    gate: Option<&ProxySelfGateMetrics>,
    allocator: &dyn Fn() -> String,
) -> String {
    let mut t = metrics.prometheus_text();
    t.push_str(&self_gate_text(gate));
    t.push_str(&sip_proxy::supervise::prometheus_text());
    t.push_str(&observe::counters::prometheus_text());
    t.push_str(&allocator());
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_proxy::observability::metrics::{CancelLookup, NoTargetDrop};

    /// The body holds its catalogue exactly, with and without a gate, with
    /// recorded counts.
    #[test]
    fn the_body_holds_its_catalogue_exactly() {
        let metrics = ProxyMetrics::new();
        metrics.record_cancel_lookup(CancelLookup::Miss);
        metrics.record_no_target_dropped(NoTargetDrop::InDialog);
        let gate = ProxySelfGateMetrics {
            elu_ewma: 0.25,
            gc_fraction: 0.0,
            cps_bucket_level: 3.0,
            cps_bucket_max: 50.0,
            external_admitted_total: 1,
            rejected_intake_age_total: 2,
            rejected_cps_total: 3,
            internal_bypassed_total: 4,
            emergency_bypassed_total: 5,
        };
        for gate in [Some(&gate), None] {
            let text = body(&metrics, gate, &jemalloc_stats::prometheus_text);
            if let Err(mismatches) = CATALOGUE.check(&text) {
                panic!("{mismatches:#?}\n{text}");
            }
        }
    }
}
