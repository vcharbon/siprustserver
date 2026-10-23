//! The reason label of `sip_proxy_hmac_failures_total`.

/// Why an HMAC verify failed, for `sip_proxy_hmac_failures_total{reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacFailureReason {
    Missing,
    Decode,
    Mismatch,
}

impl HmacFailureReason {
    /// Every reason, in declaration order.
    pub const ALL: [HmacFailureReason; 3] =
        [HmacFailureReason::Missing, HmacFailureReason::Decode, HmacFailureReason::Mismatch];

    /// The `reason` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            HmacFailureReason::Missing => "missing",
            HmacFailureReason::Decode => "decode",
            HmacFailureReason::Mismatch => "mismatch",
        }
    }
}
