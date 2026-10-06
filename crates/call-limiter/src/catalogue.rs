//! The limiter's catalogued metric families: request counters by outcome
//! and the store's own counters and gauges. Every series is `limiter_*`; no
//! per-id label.

use metric_catalogue::{Catalogue, Dim, Family, Labels};

/// An admit's answer, in slot order.
pub const ADMIT_OUTCOMES: [&str; 4] = ["admitted", "rejected", "released", "superseded"];

/// A refreshed call's answer, in slot order.
pub const REFRESH_OUTCOMES: [&str; 4] = ["extended", "reregistered", "released", "dropped"];

pub const ADMITS: Family = Family::counter(
    "limiter_admits_total",
    Labels::Product(&[Dim::new("outcome", &ADMIT_OUTCOMES)]),
    "admit requests by outcome (admitted: the whole set replaced the call's; rejected: an id the call adds is at its cap; released: the call's key is fenced by a release; superseded: the admit's change number is not above the one of the set held for the call)",
);

pub const REFRESH_REQUESTS: Family =
    Family::counter("limiter_refresh_requests_total", Labels::None, "refresh requests received");

pub const REFRESH_CALLS: Family = Family::counter(
    "limiter_refresh_calls_total",
    Labels::Product(&[Dim::new("outcome", &REFRESH_OUTCOMES)]),
    "calls the refresh requests named, by answer (extended; reregistered: a set the store no longer held re-created, with no cap check; released: refused by a release fence; dropped: refused because an admit of the key dropped its set)",
);

pub const ADMIT_REREGISTERED_CALLS: Family = Family::counter(
    "limiter_admit_reregistered_calls_total",
    Labels::None,
    "sets an admit re-created, with no cap check, from the held set it carried, for a key the store no longer held, before checking its change",
);

pub const RELEASE_REQUESTS: Family =
    Family::counter("limiter_release_requests_total", Labels::None, "release requests received");

pub const RELEASE_CALLS: Family = Family::counter(
    "limiter_release_calls_total",
    Labels::None,
    "calls the release requests named, a call named again included",
);

pub const LEASE_EXPIRED_CALLS: Family = Family::counter(
    "limiter_lease_expired_calls_total",
    Labels::None,
    "call sets dropped because their lease lapsed",
);

pub const LEASE_EXPIRED_HOLDS: Family = Family::counter(
    "limiter_lease_expired_holds_total",
    Labels::None,
    "holds the lapsed sets carried: each one a count no release freed",
);

pub const CALLS: Family = Family::gauge("limiter_calls", Labels::None, "calls holding a set");

pub const HOLDS: Family = Family::gauge(
    "limiter_holds",
    Labels::None,
    "live holds over every id (the sum of every live count)",
);

pub const FENCES: Family = Family::gauge(
    "limiter_fences",
    Labels::None,
    "keys fenced against refresh: released calls, and calls whose set an admit dropped",
);

pub const CHANGE_MARKERS: Family = Family::gauge(
    "limiter_change_markers",
    Labels::None,
    "keys holding no set whose last admit number is kept, one lease, to order later admits",
);

pub const ADMISSION_MAX: Family = Family::gauge(
    "limiter_admission_max",
    Labels::None,
    "largest live count of one id (what an admit of that id compares with its cap)",
);

/// The `/metrics` catalogue of the limiter process.
pub const CATALOGUE: Catalogue = Catalogue {
    binary: "call-limiter-runner",
    sections: &[&[
        ADMITS,
        REFRESH_REQUESTS,
        REFRESH_CALLS,
        ADMIT_REREGISTERED_CALLS,
        RELEASE_REQUESTS,
        RELEASE_CALLS,
        LEASE_EXPIRED_CALLS,
        LEASE_EXPIRED_HOLDS,
        CALLS,
        HOLDS,
        FENCES,
        CHANGE_MARKERS,
        ADMISSION_MAX,
    ]],
};
