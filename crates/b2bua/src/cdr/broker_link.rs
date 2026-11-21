//! The link of a broker CDR sink, as its metrics label it: how each connect
//! attempt ended (`b2bua_cdr_broker_connects_total{outcome}`) and why each
//! session ended (`b2bua_cdr_broker_session_ends_total{reason}`).

/// How one connect attempt of a broker sink ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerConnect {
    /// A session opened.
    Ok,
    /// No broker opened a session.
    Failed,
    /// The connect bound passed first.
    TimedOut,
}

impl BrokerConnect {
    /// Every outcome, in declaration order.
    pub const ALL: [BrokerConnect; 3] =
        [BrokerConnect::Ok, BrokerConnect::Failed, BrokerConnect::TimedOut];

    /// The `outcome` label value.
    pub const fn label(self) -> &'static str {
        match self {
            BrokerConnect::Ok => "ok",
            BrokerConnect::Failed => "failed",
            BrokerConnect::TimedOut => "timed_out",
        }
    }
}

/// Why one session of a broker sink ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerSessionEnd {
    /// The client failed a publish.
    PublishFailed,
    /// The connection took no publish within the publish bound.
    PublishBound,
    /// No confirm came within the confirm bound.
    ConfirmBound,
    /// The channel or connection failed with a confirm outstanding.
    ConfirmLost,
    /// The broker returned a publish unroutable.
    Returned,
    /// The client found the connection or channel failed or closed, between
    /// records or before a publish.
    ConnectionLost,
    /// The confirm tracker took no more publishes.
    TrackerGone,
    /// The confirm tracker panicked.
    TrackerPanic,
    /// The writer panicked during a record.
    WriterPanic,
    /// The writer closed or dropped the session.
    Closed,
}

impl BrokerSessionEnd {
    /// Every reason, in declaration order.
    pub const ALL: [BrokerSessionEnd; 10] = [
        BrokerSessionEnd::PublishFailed,
        BrokerSessionEnd::PublishBound,
        BrokerSessionEnd::ConfirmBound,
        BrokerSessionEnd::ConfirmLost,
        BrokerSessionEnd::Returned,
        BrokerSessionEnd::ConnectionLost,
        BrokerSessionEnd::TrackerGone,
        BrokerSessionEnd::TrackerPanic,
        BrokerSessionEnd::WriterPanic,
        BrokerSessionEnd::Closed,
    ];

    /// The `reason` label value.
    pub const fn label(self) -> &'static str {
        match self {
            BrokerSessionEnd::PublishFailed => "publish_failed",
            BrokerSessionEnd::PublishBound => "publish_bound",
            BrokerSessionEnd::ConfirmBound => "confirm_bound",
            BrokerSessionEnd::ConfirmLost => "confirm_lost",
            BrokerSessionEnd::Returned => "returned",
            BrokerSessionEnd::ConnectionLost => "connection_lost",
            BrokerSessionEnd::TrackerGone => "tracker_gone",
            BrokerSessionEnd::TrackerPanic => "tracker_panic",
            BrokerSessionEnd::WriterPanic => "writer_panic",
            BrokerSessionEnd::Closed => "closed",
        }
    }
}
