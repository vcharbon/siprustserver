//! The supported ranges and the defaults of a worker's tunables, which
//! [`B2buaConfig::validate`] holds the configuration to.

use super::B2buaConfig;

impl B2buaConfig {
    /// Absolute minimum OPTIONS keepalive (s). Below 2 min a mid-dialog OPTIONS
    /// poke breaks long-hold traffic (see
    /// [`keepalive_interval_sec`](Self::keepalive_interval_sec) doc). A
    /// **production** floor only — the paused-clock test harness builds configs
    /// directly and skips [`validate`](Self::validate) to use a faster cadence.
    pub const MIN_KEEPALIVE_SEC: i64 = 120;
    /// Floor for [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) (s):
    /// the out-of-dialog bound must stay above the 32 s in-dialog Timer B.
    pub const MIN_INVITE_TXN_TIMEOUT_SEC: i64 = 33;
    /// Default [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) (s):
    /// the transaction layer's own default bound,
    /// [`INVITE_INITIAL_TIMEOUT`](sip_retransmit::timers::INVITE_INITIAL_TIMEOUT).
    pub const DEFAULT_INVITE_TXN_TIMEOUT_SEC: i64 =
        sip_retransmit::timers::INVITE_INITIAL_TIMEOUT.cast_signed() / 1000;
    /// Ceiling for [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) (s):
    /// the top of the supported telephony setup range (~10 min hunting chains).
    pub const MAX_INVITE_TXN_TIMEOUT_SEC: i64 = 600;
    /// Floor for
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): two Timer A re-sends (rungs at 0.5 s and 1.5 s) precede the
    /// deadline, so a lossy path still gets more than one chance.
    pub const MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 2;
    /// Ceiling for
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): Timer B itself (RFC 3261 §17.1.1.2, 64·T1), past which the
    /// transaction reports the hop dead on its own.
    pub const MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 32;
    /// Default
    /// [`invite_first_response_timeout_sec`](Self::invite_first_response_timeout_sec)
    /// (s): three Timer A re-sends (0.5 s, 1.5 s, 3.5 s) precede it.
    pub const DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC: i64 = 5;
    /// Margin (s) a route-supplied `NoAnswer` deadline must keep under
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec): the room the
    /// CANCEL→487 exchange needs to complete inside the still-live b-leg client
    /// transaction (mirrors the default 150/158 gap).
    pub const NO_ANSWER_CANCEL_MARGIN_SEC: i64 = 8;
    /// Default [`ack_timeout_sec`](Self::ack_timeout_sec) (s): RFC 3261
    /// §13.3.1.4's `64·T1`, Timer L.
    pub const DEFAULT_ACK_TIMEOUT_SEC: i64 = 32;
}
