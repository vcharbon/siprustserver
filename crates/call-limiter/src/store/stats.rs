//! [`StoreStats`] — the store's gauges and cumulative counters.

/// A snapshot of the store's metric-relevant numbers.
#[derive(Clone, Copy, Debug)]
pub struct StoreStats {
    /// Calls holding a set.
    pub calls: u64,
    /// Sum of every live count over the ids.
    pub current_total: i64,
    /// The largest live count of one id: what the next admit of that id
    /// compares with its cap. 0 when nothing is held.
    pub admission_max: i64,
    /// Keys fenced against refresh: released calls, and calls whose set an
    /// admit dropped.
    pub fences: u64,
    /// Keys holding no set whose last admit number is kept to order later
    /// admits.
    pub change_markers: u64,
    /// Cumulative sets dropped because their lease lapsed.
    pub lease_expired_calls: u64,
    /// Cumulative holds those sets carried.
    pub lease_expired_holds: u64,
    /// Cumulative sets re-created by a refresh of a call the store no longer
    /// held.
    pub reregistered_calls: u64,
    /// Cumulative sets re-created by an admit, from the held set it carried,
    /// for a key the store no longer held.
    pub admit_reregistered_calls: u64,
    /// Cumulative release calls, no-ops included.
    pub releases_total: u64,
    /// Cumulative admits refused because the call was released.
    pub admits_refused_released: u64,
}
