//! The families of the load balancer's per-worker overload path: new dialogs
//! it refuses, spills past a rate-capped winner, or lets emergency traffic
//! bypass, and the stale-payload floor of a worker's AIMD cap.

use metric_catalogue::{Dim, Family, Labels};

pub const OVERLOAD_REJECTIONS: Family = Family::counter(
    "sip_proxy_overload_rejections_total",
    Labels::Product(&[Dim::new("reason", &["no_target_critical_filtered", "bucket_empty"])]),
    "New-dialog admissions rejected by the AIMD/band overload path, by reason.",
);

pub const WORKER_STALE_DECREASE: Family = Family::counter(
    "sip_proxy_worker_stale_decrease_total",
    Labels::None,
    "WorkerLoadObserver sweep_stale floor events (AIMD cap halved — no fresh X-Overload within payload_stale_ms).",
);

pub const LB_EMERGENCY_BYPASSED: Family = Family::counter(
    "sip_proxy_lb_emergency_bypassed_total",
    Labels::None,
    "Emergency new-dialog INVITEs that bypassed the LB overload gates (above_critical critical-filter + per-worker AIMD bucket). Emergency traffic skipping the load-balancer's overload path under flood.",
);

pub const LB_RATE_CAP_SPILLED: Family = Family::counter(
    "sip_proxy_lb_rate_cap_spilled_total",
    Labels::None,
    "New dialogs placed on a later rendezvous candidate because the winner's per-worker AIMD bucket was empty.",
);
