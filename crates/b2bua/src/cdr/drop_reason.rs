//! Why a record handed to a CDR sink ended dropped: the `reason` label of
//! `b2bua_cdr_dropped_total`. Each drop site counts one reason, so an
//! operator reads the cause of a drop from the counter alone.

/// Why one record ended dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdrDrop {
    /// The buffer's submit queue was full.
    BufferFull,
    /// The record was handed to the sink after its close began.
    Closed,
    /// The close deadline passed with the record queued, being written or
    /// awaiting its confirm.
    CloseDeadline,
    /// The encoder refused the call.
    Encode,
    /// No broker session could publish: the reconnect backoff held, or the
    /// connect attempt failed or timed out.
    NoBroker,
    /// Every slot of the confirm window stayed taken past the publish bound.
    WindowFull,
    /// The broker session ended before or during the publish.
    PublishFailed,
    /// The broker nacked the publish.
    Nacked,
    /// The broker returned the publish unroutable.
    Returned,
    /// No confirm came: the confirm bound passed, the connection was lost, or
    /// the session ended with the confirm outstanding.
    Unconfirmed,
    /// The file refused the record's line.
    FileWrite,
    /// The sink, its drainer or its confirm tracker panicked holding the
    /// record, or the drainer gave its queue up after two panics in a close.
    Panic,
}

impl CdrDrop {
    /// Every reason, in declaration order.
    pub const ALL: [CdrDrop; 12] = [
        CdrDrop::BufferFull,
        CdrDrop::Closed,
        CdrDrop::CloseDeadline,
        CdrDrop::Encode,
        CdrDrop::NoBroker,
        CdrDrop::WindowFull,
        CdrDrop::PublishFailed,
        CdrDrop::Nacked,
        CdrDrop::Returned,
        CdrDrop::Unconfirmed,
        CdrDrop::FileWrite,
        CdrDrop::Panic,
    ];

    /// The `reason` label value.
    pub const fn label(self) -> &'static str {
        match self {
            CdrDrop::BufferFull => "buffer_full",
            CdrDrop::Closed => "closed",
            CdrDrop::CloseDeadline => "close_deadline",
            CdrDrop::Encode => "encode",
            CdrDrop::NoBroker => "no_broker",
            CdrDrop::WindowFull => "window_full",
            CdrDrop::PublishFailed => "publish_failed",
            CdrDrop::Nacked => "nacked",
            CdrDrop::Returned => "returned",
            CdrDrop::Unconfirmed => "unconfirmed",
            CdrDrop::FileWrite => "file_write",
            CdrDrop::Panic => "panic",
        }
    }
}
