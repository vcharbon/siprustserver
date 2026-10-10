use super::*;

/// [`states_send_time`] names the two clock stamps of RFC 3261 and nothing
/// else, in any casing.
#[test]
fn the_predicate_names_exactly_the_clock_stamps() {
    for name in ["Date", "date", "Timestamp"] {
        assert!(states_send_time(name), "{name} states send time");
    }
    for name in ["Allow", "Supported", "X-Vendor-Term", "Content-Disposition", "Session-Expires"] {
        assert!(!states_send_time(name), "{name} does not state send time");
    }
}
