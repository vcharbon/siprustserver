//! The reason label of `sip_proxy_hmac_failures_total`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacFailureReason {
    Missing,
    Decode,
    Mismatch,
}

impl HmacFailureReason {
    pub const ALL: [HmacFailureReason; 3] =
        [HmacFailureReason::Missing, HmacFailureReason::Decode, HmacFailureReason::Mismatch];

    pub const fn as_str(self) -> &'static str {
        match self {
            HmacFailureReason::Missing => "missing",
            HmacFailureReason::Decode => "decode",
            HmacFailureReason::Mismatch => "mismatch",
        }
    }
}
