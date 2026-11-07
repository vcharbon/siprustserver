//! The families of the wire messages that went nowhere: outbound ones that could
//! not leave and inbound ones or own events that named no call
//! (`crate::metrics::wire_drops`).

use metric_catalogue::{Dim, Family, Labels};

use super::worker::ANSWERED_METHOD;

/// What an unroutable message that drew no answer was: an ACK, or a
/// response by status class.
pub const UNROUTABLE_KIND: Dim =
    Dim::new("kind", &["ACK", "1xx", "2xx", "3xx", "4xx", "5xx", "6xx"]);

pub const OUTBOUND_UNSENT: Family = Family::counter(
    "b2bua_outbound_unsent_total",
    Labels::Union(&[
        &[
            Dim::new("reason", &["unaddressable"]),
            Dim::new("kind", &["request", "response", "datagram"]),
        ],
        &[Dim::new("reason", &["too_long"]), Dim::new("kind", &["request"])],
    ]),
    "outbound SIP messages this worker did not hand to its socket, by reason and kind: unaddressable, the next hop is not an IP literal (the worker resolves no name); too_long, a request opening a client transaction is past the largest UDP payload. Such a request fails its call's attempt at once, as a dead hop's",
);

pub const UNROUTABLE_DROPPED: Family = Family::counter(
    "b2bua_unroutable_dropped_total",
    Labels::Product(&[UNROUTABLE_KIND]),
    "wire messages that resolved to no call and drew no answer, by kind (ACK, or a response's status class); nothing a peer waits on",
);

pub const UNROUTABLE_REFUSED: Family = Family::counter(
    "b2bua_unroutable_refused_total",
    Labels::Product(&[ANSWERED_METHOD, Dim::new("code", &["481"])]),
    "wire requests that resolved to no call, answered on no call's behalf (481 RFC 3261 §12.2.2/§15.1.2/§9.2, 405 §8.2.1) by method and code; a stray CANCEL's 481 is stateless, so each repeat of it counts again",
).capped(&UNROUTABLE_REFUSED_OVERFLOW, &["method"]);

pub const UNROUTABLE_INTERNAL: Family = Family::counter(
    "b2bua_unroutable_internal_total",
    Labels::Union(&[
        &[Dim::new("event", &["timeout"]), ANSWERED_METHOD],
        &[
            Dim::new("event", &["timeout", "timer", "cancelled", "internal-event"]),
            Dim::new("method", &[""]),
        ],
    ]),
    "this node's own events that resolved to no call, by event and method (timeout: a client transaction released from its call reached Timer B/F); no wire message and no peer waiting",
).capped(&UNROUTABLE_INTERNAL_OVERFLOW, &["method"]);

pub const UNROUTABLE_REFUSED_OVERFLOW: Family = Family::counter(
    "b2bua_unroutable_refused_overflow_total",
    Labels::None,
    "observations of b2bua_unroutable_refused_total past its cap, each counted on its series whose method reads _overflow",
);

pub const UNROUTABLE_INTERNAL_OVERFLOW: Family = Family::counter(
    "b2bua_unroutable_internal_overflow_total",
    Labels::None,
    "observations of b2bua_unroutable_internal_total past its cap, each counted on its series whose method reads _overflow",
);
