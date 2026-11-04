//! A new call's setup times (ADR-0022 X1): its turn's wait from the INVITE's
//! receipt and its hold of the handler permits.

use metric_catalogue::{Family, Labels};

/// The upper bounds (seconds) of both families' buckets, ascending.
pub const BUCKETS: [f64; 10] = [0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0];

pub const TURN_WAIT_SECONDS: Family = Family::histogram(
    "b2bua_new_call_turn_wait_seconds",
    Labels::None,
    "time from a new initial INVITE's receipt by the transaction layer to the start of its turn, the wait for a handler permit included; a turn at B2BUA_CALL_CONTROL_TIMEOUT_MS is refused unasked (ADR-0022 X1)",
);

pub const PERMIT_HOLD_SECONDS: Family = Family::histogram(
    "b2bua_new_call_permit_hold_seconds",
    Labels::None,
    "time a new initial INVITE's turn held its handler permits, its decision round trips and limiter admits included: with the new-call share and the rate it sets whether new calls queue for a permit (ADR-0022 X1)",
);
