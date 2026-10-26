//! How much the branch memo holds: [`MemoBounds`], its defaults, and the
//! share of them each [`Lifetime`] holds.

use super::{Lifetime, INVITE_ENTRY_TTL_MS, RTX_ENTRY_TTL_MS};

/// The entry-milliseconds one call holds: an INVITE entry for each leg's
/// INVITE, and a retransmission memo for each leg's INVITE, ACK and BYE.
pub const ENTRY_MS_PER_CALL: u64 = 2 * INVITE_ENTRY_TTL_MS + 6 * RTX_ENTRY_TTL_MS;

/// The INVITE entries' part of [`ENTRY_MS_PER_CALL`], the share of the bounds
/// the INVITE-transaction pool holds.
const INVITE_MS_PER_CALL: u64 = 2 * INVITE_ENTRY_TTL_MS;

/// The default byte budget: 3/8 of the front proxy's 512 MiB memory limit.
pub const DEFAULT_MAX_BYTES: usize = 192 << 20;

/// The bytes the memo accounts for the heaviest entry the request path
/// writes: a worker's INVITE retransmission memo carrying both the cookie of
/// the Record-Route it minted and the cookie of its Via.
pub const TYPICAL_ENTRY_BYTES: usize = 2_560;

/// The default entry cap: what [`DEFAULT_MAX_BYTES`] holds of
/// [`TYPICAL_ENTRY_BYTES`] entries.
pub const DEFAULT_MAX_ENTRIES: usize = DEFAULT_MAX_BYTES / TYPICAL_ENTRY_BYTES;

/// The independently locked stripes of a default memo.
pub const DEFAULT_STRIPES: usize = 16;

/// The bounds of a branch memo, on the bytes it accounts for its entries,
/// not on the memory the process holds. Each of the `stripes` holds its
/// share of both bounds, rounded up, and splits it between its two
/// [`Lifetime`] pools as a call's entry-milliseconds split
/// ([`ENTRY_MS_PER_CALL`]): the INVITE-transaction pool about two thirds, the
/// retransmission pool the rest. Past a bound a pool forgets its oldest write
/// first, and never an entry of the other pool. The defaults hold the working
/// set of 109 new calls a second, every entry taken at the heaviest shape,
/// with a quarter of headroom ([`calls_per_second`]).
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

    /// One stripe's `lifetime` pool: `(max_entries, max_bytes)`, each at
    /// least one.
    pub(super) fn per_pool(&self, lifetime: Lifetime) -> (usize, usize) {
        let n = self.stripe_count();
        let split = |total: usize| {
            let stripe = total.div_ceil(n);
            let invite = invite_share(stripe);
            match lifetime {
                Lifetime::InviteTransaction => invite.max(1),
                Lifetime::Retransmission => stripe.saturating_sub(invite).max(1),
            }
        };
        (split(self.max_entries), split(self.max_bytes))
    }
}

/// The INVITE-transaction pool's part of `n`.
fn invite_share(n: usize) -> usize {
    let whole = usize::try_from(ENTRY_MS_PER_CALL).unwrap_or(usize::MAX);
    let part = usize::try_from(INVITE_MS_PER_CALL).unwrap_or(0);
    (n / whole).saturating_mul(part).saturating_add((n % whole).saturating_mul(part) / whole)
}

/// The new calls a second whose entries `bounds` holds with a quarter of
/// headroom, each call holding [`ENTRY_MS_PER_CALL`] of entries.
pub fn calls_per_second(bounds: &MemoBounds) -> usize {
    let call_seconds = usize::try_from(ENTRY_MS_PER_CALL / 1_000).unwrap_or(usize::MAX);
    bounds.max_entries.saturating_mul(4) / call_seconds.saturating_mul(5).max(1)
}
