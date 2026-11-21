//! The dispatcher's label tables: where a discarded INVITE was answered and
//! which bound an item was queued past, each in label order.

use super::RemovalClass;
use crate::dispatch::{Discard, PastBound};

/// One discard of each site, in label order: a release of any class counts
/// under `released`.
pub(crate) const DISCARD_SITES: [Discard; 4] = [
    Discard::QueueFull,
    Discard::AtCap,
    Discard::Released(RemovalClass::Terminated),
    Discard::Capped,
];

pub(crate) const fn discard_label(why: Discard) -> &'static str {
    match why {
        Discard::QueueFull => "queue_full",
        Discard::AtCap => "at_cap",
        Discard::Released(_) => "released",
        Discard::Capped => "capped",
    }
}

/// Every dispatcher bound, in label order.
pub(crate) const PAST_BOUNDS: [PastBound; 2] = [PastBound::Depth, PastBound::Cap];

pub(crate) const fn past_bound_label(bound: PastBound) -> &'static str {
    match bound {
        PastBound::Depth => "depth",
        PastBound::Cap => "cap",
    }
}
