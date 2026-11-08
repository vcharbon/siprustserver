//! The families of the per-call caps (ADR-0057 §7): each trip by cap and
//! kind, and each crossing of half a cap.

use metric_catalogue::{Dim, Family, Labels};

/// The caps a peer's traffic can reach, in [`b2bua_sdk::config::Cap::ALL`] order.
const PEER_CAPS: [&str; 4] = ["legs", "requests", "cdr-events", "dialogs"];
/// The caps only a defect reaches, in [`b2bua_sdk::config::Cap::ALL`] order.
const DEFECT_CAPS: [&str; 5] =
    ["attempts", "timers", "header-updates", "ext-bytes", "actions-per-turn"];
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
    }
}
