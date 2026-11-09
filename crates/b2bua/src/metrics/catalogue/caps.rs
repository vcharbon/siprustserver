//! The families of the per-call caps (ADR-0057 §7, §9): each trip by cap and
//! kind, each crossing of half a cap, and the largest reading of each cap a
//! landed turn left.

use metric_catalogue::{Dim, Family, Labels};

/// The caps a peer's traffic can reach, in [`b2bua_sdk::config::Cap::ALL`] order.
const PEER_CAPS: [&str; 4] = ["legs", "requests", "cdr-events", "dialogs"];
/// The caps only a defect reaches, in [`b2bua_sdk::config::Cap::ALL`] order.
const DEFECT_CAPS: [&str; 5] =
    ["attempts", "timers", "header-updates", "ext-bytes", "actions-per-turn"];
/// Every cap, in [`b2bua_sdk::config::Cap::ALL`] order.
const ALL_CAPS: [&str; 9] = [
    "legs",
    "attempts",
    "timers",
    "requests",
    "cdr-events",
    "dialogs",
    "header-updates",
    "ext-bytes",
    "actions-per-turn",
];
/// The caps on what grows with a call's age, in [`b2bua_sdk::config::Cap::AGED`] order.
const AGED_CAPS: [&str; 3] = ["legs", "attempts", "cdr-events"];
/// The counted collections' caps, in [`b2bua_sdk::config::Cap::ALL`] order.
const COUNTED_CAPS: [&str; 7] =
    ["legs", "attempts", "timers", "requests", "cdr-events", "dialogs", "header-updates"];

pub const CAP_TRIPS: Family = Family::counter(
    "b2bua_cap_trips_total",
    Labels::Union(&[
        &[Dim::new("cap", &PEER_CAPS), Dim::new("kind", &["peer"])],
        &[Dim::new("cap", &DEFECT_CAPS), Dim::new("kind", &["defect"])],
    ]),
    "turns that crossed a per-call cap (ADR-0057), by cap and kind: the turn is discarded and its call ends from the record it had before the event, torn down on the wire. One increment is one tripped turn, not one call: a call may trip again before its teardown runs. A defect cap is reached only by a defect in a service, the decision service or the B2BUA, and alerts; a peer cap is reachable by a peer's traffic",
);

pub const CAP_NEAR: Family = Family::counter(
    "b2bua_cap_near_total",
    Labels::Product(&[Dim::new("cap", &COUNTED_CAPS)]),
    "turns that landed having grown a call's counted collection across half its cap (ADR-0057 §7), by cap; one increment per crossing. A service's ext value, measured once per turn, has none",
);

pub const CAP_PEAK: Family = Family::gauge(
    "b2bua_cap_peak",
    Labels::Product(&[Dim::new("cap", &ALL_CAPS)]),
    "the largest reading of each per-call cap since the process started (ADR-0057 §9), by cap: what a turn held to its caps left its call holding (a collection's length, a service's encoded ext value in bytes) and the actions it returned. A tripped turn reads nothing, nor does the router's own ending of a call. Each default is the largest reading over valid traffic times a factor set by what the cap costs: 20 for what a call holds at once, 100 for legs, 1000 for attempts, hundreds and more for the fuel; a cap on what grows with a call's age is a base and a rate (b2bua_cap_peak_per_hour)",
);

pub const CAP_PEAK_PER_HOUR: Family = Family::gauge(
    "b2bua_cap_peak_per_hour",
    Labels::Product(&[Dim::new("cap", &AGED_CAPS)]),
    "the largest reading of each cap on what grows with a call's age, per started hour of the call's granted duration, since the process started (ADR-0057 §9), by cap; read once a duration is granted. A call granted an hour or less reads its whole length here. Sets the cap's `_PER_HOUR` rate",
);

#[cfg(test)]
mod tests {
    use b2bua_sdk::config::{Cap, CapKind};

    #[test]
    fn the_label_sets_name_every_cap_by_its_kind() {
        let of = |kind| Cap::ALL.into_iter().filter(move |c| c.kind() == kind).map(Cap::label);
        assert!(of(CapKind::Peer).eq(super::PEER_CAPS));
        assert!(of(CapKind::Defect).eq(super::DEFECT_CAPS));
        let counted =
            Cap::ALL.into_iter().filter(|c| !matches!(c, Cap::ActionsPerTurn | Cap::ExtBytes));
        assert!(counted.map(Cap::label).eq(super::COUNTED_CAPS));
        assert!(Cap::ALL.into_iter().map(Cap::label).eq(super::ALL_CAPS));
        assert!(Cap::AGED.into_iter().map(Cap::label).eq(super::AGED_CAPS));
    }
}
