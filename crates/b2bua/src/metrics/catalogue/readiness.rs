//! The readiness families: the operator's cordon and the event-queue gate,
//! each a reason a worker that serves its dialogs refuses new calls
//! (ADR-0050 §9).

use metric_catalogue::{Family, Labels};

pub const CORDONED: Family = Family::gauge(
    "b2bua_cordoned",
    Labels::None,
    "1 while an operator cordon is set on this worker (POST /admin/cordon, lifted by DELETE): it refuses new calls with its own 503 and serves every dialog; a worker that drains keeps the cordon it was given (ADR-0050 §9)",
);

pub const EVENT_QUEUE_REFUSING: Family = Family::gauge(
    "b2bua_event_queue_refusing",
    Labels::None,
    "1 while this worker refuses new calls because its transaction event queue is backed up, by depth or by the wait of its events (X-Readiness: new=refuse; cause=overload, ADR-0050 §9); it serves every dialog",
);

pub const EVENT_QUEUE_REFUSALS: Family = Family::counter(
    "b2bua_event_queue_refusals_total",
    Labels::None,
    "times this worker turned to refusing new calls on a backed-up transaction event queue",
);
