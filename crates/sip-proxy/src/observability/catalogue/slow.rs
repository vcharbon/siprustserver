//! The families of the slow axis of worker health (ADR-0069): each worker's
//! state, its late probe answers, its `Slow` spells and the dead verdicts a
//! spell too long draws.

use metric_catalogue::{Dim, Family, Labels};

use super::HEALTH_STATES;

/// A worker's ordinal: no value declared; each judged worker gets its own
/// series.
const WORKER: Dim = Dim::new("worker", &[]);

pub const WORKER_STATE: Family = Family::gauge(
    "sip_proxy_worker_health",
    Labels::Product(&[WORKER, Dim::new("state", &HEALTH_STATES)]),
    "1 on each routed worker's health state (alive, slow, unknown, dead), 0 on the others; a worker that leaves the routing drops out.",
)
.semi_open();

pub const WORKER_SLOW: Family = Family::counter(
    "sip_proxy_worker_slow_total",
    Labels::Product(&[WORKER]),
    "Slow spells begun per worker: it missed HEALTH_THRESHOLD probes but answered one late, inside HEALTH_SLOW_WINDOW_MS of its send, or its endpoint flipped not ready while it did. A slow worker serves its dialogs and takes no new one.",
)
.semi_open();

pub const PROBE_LATE_ANSWERS: Family = Family::counter(
    "sip_proxy_probe_late_answers_total",
    Labels::Product(&[WORKER]),
    "Probe answers per worker that arrived past their reply window (HEALTH_TIMEOUT_MS) and inside the slow window (HEALTH_SLOW_WINDOW_MS) of their send.",
)
.semi_open();

pub const SLOW_MAX_DEAD: Family = Family::counter(
    "sip_proxy_slow_max_dead_total",
    Labels::Product(&[WORKER]),
    "Dead verdicts per worker drawn by HEALTH_SLOW_MAX_MS spent slow in all: its calls are taken over while it may still run.",
)
.semi_open();
