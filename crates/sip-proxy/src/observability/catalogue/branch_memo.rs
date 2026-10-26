//! The families of the CANCEL/ACK/retransmission branch memo's bounds
//! (`crate::cancel_lru`).

use metric_catalogue::{Family, Labels};

pub const BRANCH_MEMO_BYTES: Family = Family::gauge(
    "sip_proxy_branch_memo_bytes",
    Labels::None,
    "Bytes the CANCEL/ACK/retransmission branch memo accounts for its entries, against its byte budget.",
);

pub const BRANCH_MEMO_EVICTIONS: Family = Family::counter(
    "sip_proxy_branch_memo_evictions_total",
    Labels::None,
    "Branch memo entries forgotten before their expiry because the memo was at its entry cap or byte budget; a CANCEL, non-2xx ACK or retransmission whose entry went is routed as one never seen.",
);
