//! The CDR sink's families: each record's end, written or dropped by reason,
//! and a broker sink's link (its live session, its connect attempts and why
//! its sessions ended).

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};

use crate::cdr::{BrokerConnect, BrokerSessionEnd, CdrDrop};

const DROP_REASON_VALUES: [&str; CdrDrop::ALL.len()] = label_values!(CdrDrop::ALL, CdrDrop::label);
/// Why a record was dropped, indexed like [`CdrDrop::ALL`].
pub const DROP_REASON: Dim = Dim::new("reason", &DROP_REASON_VALUES);

const CONNECT_OUTCOME_VALUES: [&str; BrokerConnect::ALL.len()] =
    label_values!(BrokerConnect::ALL, BrokerConnect::label);
/// How a connect attempt ended, indexed like [`BrokerConnect::ALL`].
pub const CONNECT_OUTCOME: Dim = Dim::new("outcome", &CONNECT_OUTCOME_VALUES);

const SESSION_END_VALUES: [&str; BrokerSessionEnd::ALL.len()] =
    label_values!(BrokerSessionEnd::ALL, BrokerSessionEnd::label);
/// Why a broker session ended, indexed like [`BrokerSessionEnd::ALL`].
pub const SESSION_END: Dim = Dim::new("reason", &SESSION_END_VALUES);

assert_exposition_order!(
    CdrDrop: BufferFull,
    Closed,
    CloseDeadline,
    Encode,
    NoBroker,
    WindowFull,
    PublishFailed,
    Nacked,
    Returned,
    Unconfirmed,
    FileWrite,
    Panic,
);
assert_exposition_order!(BrokerConnect: Ok, Failed, TimedOut);
assert_exposition_order!(
    BrokerSessionEnd: PublishFailed,
    PublishBound,
    ConfirmBound,
    Returned,
    ConnectionLost,
    TrackerGone,
    TrackerPanic,
    WriterPanic,
    Closed,
);

pub const WRITTEN: Family = Family::counter(
    "b2bua_cdr_written_total",
    Labels::None,
    "CDRs the sink delivered (for a broker sink: acked by the broker)",
);

pub const DROPPED: Family = Family::counter(
    "b2bua_cdr_dropped_total",
    Labels::Product(&[DROP_REASON]),
    "CDRs that ended undelivered, by reason: buffer_full (the submit queue was full), closed (handed to the sink after its close began), close_deadline (queued, being written or awaiting its confirm when the close deadline passed), encode (the encoder refused the call), no_broker (no broker session: the reconnect backoff, or a failed or timed-out connect), window_full (the confirm window stayed full past the publish bound), publish_failed (the session ended before or during the publish), nacked, returned (unroutable), unconfirmed (no confirm: the confirm bound passed, the connection was lost or the session ended first; the record may still reach the queue), file_write (the file refused the line), panic (the sink, its drainer or its confirm tracker panicked). Each record handed to the sink counts once, here or in b2bua_cdr_written_total",
);

pub const BROKER_CONNECTED: Family = Family::gauge(
    "b2bua_cdr_broker_connected",
    Labels::None,
    "1 while the broker CDR sink holds a live session, 0 before its first connect and from a session's end to the next session's open; a channel the broker closes without an error, its connection kept, reads live until the next record finds it closed",
);

pub const BROKER_CONNECTS: Family = Family::counter(
    "b2bua_cdr_broker_connects_total",
    Labels::Product(&[CONNECT_OUTCOME]),
    "connect attempts of the broker CDR sink, by outcome: ok (a session opened), failed (no listed broker opened one), timed_out (the connect bound passed first); an attempt is made at a record, after the reconnect backoff",
);

pub const BROKER_SESSION_ENDS: Family = Family::counter(
    "b2bua_cdr_broker_session_ends_total",
    Labels::Product(&[SESSION_END]),
    "broker sessions of the CDR sink ended, by reason: publish_failed (the client failed a publish), publish_bound (the connection took no publish within the publish bound), confirm_bound (no confirm within the confirm bound), returned (a publish came back unroutable), connection_lost (the connection or channel failed or closed: between records, before a publish or with a confirm outstanding), tracker_gone, tracker_panic (the confirm tracker stopped or panicked), writer_panic (the writer panicked holding the session; its publishes still resolve), closed (the writer closed it)",
);
