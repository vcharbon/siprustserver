//! The families of the CANCEL/ACK/retransmission branch memo's bounds
//! (`crate::cancel_lru`), read from the memo at the scrape.

use metric_catalogue::{Family, Labels};

pub const BRANCH_MEMO_BYTES: Family = Family::gauge(
    "sip_proxy_branch_memo_bytes",
    Labels::None,
    "Bytes the CANCEL/ACK/retransmission branch memo accounts for its entries (their allocations, not resident memory), against its byte budget.",
);

pub const BRANCH_MEMO_EVICTIONS: Family = Family::counter(
    "sip_proxy_branch_memo_evictions_total",
    Labels::None,
    "Branch memo entries forgotten before their expiry because the memo was at its entry cap or byte budget; a CANCEL, non-2xx ACK or retransmission whose entry went is routed as one never seen.",
);

pub const BRANCH_MEMO_DEVIATIONS: Family = Family::counter(
    "sip_proxy_branch_memo_deviations_total",
    Labels::None,
    "INVITEs placed off their memo-free route (a spill past an empty bucket, a band-filtered winner, a winner refusing new calls): the only INVITE entries the branch memo holds; its rate times the ring time sizes the INVITE entries.",
);

/// The branch memo, as `CancelBranchLru::prometheus_text` writes it.
pub const BRANCH_MEMO: &[Family] =
    &[BRANCH_MEMO_BYTES, BRANCH_MEMO_EVICTIONS, BRANCH_MEMO_DEVIATIONS];
