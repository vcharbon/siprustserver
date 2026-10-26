//! How much the branch memo holds: [`MemoBounds`], its defaults sized from
//! the call rate the proxy is built for, and the bytes one entry accounts.

use std::mem::size_of;
use std::sync::Arc;

use super::{CancelEntry, INVITE_ENTRY_TTL_MS, RTX_ENTRY_TTL_MS};
use crate::strategy::RouteParams;

/// The new-call rate whose whole working set the default bounds hold.
pub const DESIGN_CPS: usize = 2_048;

/// The entry-milliseconds one call holds: an INVITE entry for each leg's
/// INVITE, and a short memo for each leg's INVITE, ACK and BYE.
pub const ENTRY_MS_PER_CALL: u64 = 2 * INVITE_ENTRY_TTL_MS + 6 * RTX_ENTRY_TTL_MS;

/// The default entry cap: [`DESIGN_CPS`] calls a second, each holding
/// [`ENTRY_MS_PER_CALL`], with a quarter of headroom.
pub const DEFAULT_MAX_ENTRIES: usize = DESIGN_CPS * (ENTRY_MS_PER_CALL / 1_000) as usize / 4 * 5;

/// The bytes the memo accounts for an entry of ordinary length (an RFC 3261
/// branch, a Call-ID of a few dozen characters, a stickiness cookie).
pub const TYPICAL_ENTRY_BYTES: usize = 512;

/// The default byte budget: a full table of ordinary entries, so keys made
/// as long as the parser admits buy no more memory than ordinary ones.
pub const DEFAULT_MAX_BYTES: usize = DEFAULT_MAX_ENTRIES * TYPICAL_ENTRY_BYTES;

/// The independently locked stripes of a default memo.
pub const DEFAULT_STRIPES: usize = 16;

/// The bytes an entry costs beside its strings: its slot, the key shared by
/// the table and the expiry index, the index cell, and the containers'
/// per-entry bookkeeping, approximated.
const ENTRY_OVERHEAD_BYTES: usize =
    size_of::<CancelEntry>() + 3 * size_of::<Arc<str>>() + 4 * size_of::<u64>() + 64;

/// The bounds of a branch memo. Past either bound a stripe forgets the entry
/// nearest its expiry first, so a ringing INVITE's entry outlives a flood of
/// short-lived memos. Each of the `stripes` holds its share of both bounds,
/// rounded up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoBounds {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub stripes: usize,
}

impl Default for MemoBounds {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            stripes: DEFAULT_STRIPES,
        }
    }
}

impl MemoBounds {
    /// The stripe count, at least one.
    pub(super) fn stripe_count(&self) -> usize {
        self.stripes.max(1)
    }

    /// One stripe's `(max_entries, max_bytes)`.
    pub(super) fn per_stripe(&self) -> (usize, usize) {
        let n = self.stripe_count();
        (self.max_entries.div_ceil(n), self.max_bytes.div_ceil(n))
    }
}

/// The bytes the memo accounts for `entry` under `key`: every string it
/// holds, plus [`ENTRY_OVERHEAD_BYTES`].
pub(super) fn footprint(key: &str, entry: &CancelEntry) -> usize {
    [
        key.len(),
        entry.branch.len(),
        entry.target.host.len(),
        params(entry.stickiness.as_ref()),
        params(entry.minted.as_ref()),
        params(entry.via_cookie.as_ref()),
        ENTRY_OVERHEAD_BYTES,
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

fn params(params: Option<&RouteParams>) -> usize {
    params
        .into_iter()
        .flatten()
        .map(|(name, value)| name.len().saturating_add(value.len()))
        .fold(0, usize::saturating_add)
}
