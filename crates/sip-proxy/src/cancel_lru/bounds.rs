//! How much the branch memo holds and how long an INVITE entry may live:
//! [`MemoBounds`], its defaults, and the share of them each [`Lifetime`]
//! holds.

use sip_txn::timers::{INVITE_INITIAL_TIMEOUT, TIMER_H};

use super::{Lifetime, RTX_ENTRY_TTL_MS};

/// The cap of an INVITE entry whose final this proxy never relays, for an
/// INVITE transaction bound of `bound_sec` downstream: the whole window the
/// downstream UA may ring, plus the final's retransmission tail (Timer H).
/// An entry under it would lose the CANCEL's target and the non-2xx ACK's hop
/// while the callee still legally rings.
pub const fn invite_entry_ttl_ms(bound_sec: u64) -> u64 {
    bound_sec.saturating_mul(1000).saturating_add(TIMER_H)
}

/// [`invite_entry_ttl_ms`] at the transaction layer's default bound.
pub const INVITE_ENTRY_TTL_MS: u64 = INVITE_INITIAL_TIMEOUT + TIMER_H;

/// The default mean ring, ms, the bounds are sized on: the time an INVITE
/// entry waits for its final, on average.
pub const DEFAULT_MEAN_RING_MS: u64 = 30_000;

/// The default INVITEs per call, thousandths: a basic call's two, one per
/// leg.
pub const DEFAULT_INVITES_PER_CALL_MILLI: u64 = 2_000;

/// The entry-milliseconds one call holds at one instance, at most, under
/// `bounds`: the INVITE-transaction pool's part ([`MemoBounds::invite_ms_per_call`])
/// and the retransmission pool's. A basic call leaves there Timer H for each
/// leg's INVITE entry its final shortened (ADR-0009 X7), a retransmission
/// memo for each leg's INVITE, ACK and BYE and for one keepalive OPTIONS per
/// leg, and one ACK hop for a relayed non-2xx final; each further INVITE
/// leaves three more: its INVITE's and its ACK's retransmission memos, and its
/// ACK hop or its shortened entry.
pub const fn entry_ms_per_call(bounds: &MemoBounds) -> u64 {
    let further = bounds.further_invites_milli();
    bounds.invite_ms_per_call()
        + 2 * TIMER_H
        + 9 * RTX_ENTRY_TTL_MS
        + further.saturating_mul(TIMER_H + 2 * RTX_ENTRY_TTL_MS) / 1_000
}

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
/// [`Lifetime`] pools as a call's entry-milliseconds split at the mean ring
/// and the INVITE entry cap ([`entry_ms_per_call`]). Past a bound a pool
/// forgets its oldest write first, and never an entry of the other pool. The
/// defaults hold the working set of 109 new calls a second at a 30 s mean
/// ring and the default INVITE bound, every entry taken at the heaviest
/// shape, with a quarter of headroom ([`calls_per_second`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoBounds {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub stripes: usize,
    /// The cap of an INVITE entry, ms ([`invite_entry_ttl_ms`] of the
    /// downstream INVITE transaction bound).
    pub invite_ttl_ms: u64,
    /// The mean ring the pools are split on, ms.
    pub mean_ring_ms: u64,
    /// The INVITEs that cross the pair per call on average, thousandths, at
    /// least a basic call's two: re-INVITEs and the further legs of a
    /// rerouted or transferred call each write an entry.
    pub invites_per_call_milli: u64,
}

impl Default for MemoBounds {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            stripes: DEFAULT_STRIPES,
            invite_ttl_ms: INVITE_ENTRY_TTL_MS,
            mean_ring_ms: DEFAULT_MEAN_RING_MS,
            invites_per_call_milli: DEFAULT_INVITES_PER_CALL_MILLI,
        }
    }
}

impl MemoBounds {
    /// The INVITEs per call beyond a basic call's two, thousandths.
    const fn further_invites_milli(&self) -> u64 {
        self.invites_per_call_milli.saturating_sub(DEFAULT_INVITES_PER_CALL_MILLI)
    }

    /// The INVITE-transaction pool's part of [`entry_ms_per_call`]. An entry
    /// whose final crosses the other instance of an active pair lives to its
    /// cap. Of a call's two basic legs, at most one crosses at this instance:
    /// the B2BUA sends the caller leg's final and the callee leg's INVITE on
    /// one flow, so when that flow lands elsewhere the caller leg's entry
    /// crosses here and the callee leg's INVITE never passes here, and when
    /// it lands here the caller leg's entry is shortened after the ring. Each
    /// further INVITE may cross to its cap.
    pub const fn invite_ms_per_call(&self) -> u64 {
        let further = self.further_invites_milli();
        self.invite_ttl_ms
            .saturating_add(self.mean_ring_ms)
            .saturating_add(further.saturating_mul(self.invite_ttl_ms) / 1_000)
    }

    /// The stripe count, at least one.
    pub(super) fn stripe_count(&self) -> usize {
        self.stripes.max(1)
    }

    /// How long an entry of `lifetime` lives.
    pub(super) fn ttl_ms(&self, lifetime: Lifetime) -> u64 {
        match lifetime {
            Lifetime::InviteTransaction => self.invite_ttl_ms,
            Lifetime::Retransmission => RTX_ENTRY_TTL_MS,
        }
    }

    /// One stripe's `lifetime` pool: `(max_entries, max_bytes)`, each at
    /// least one.
    pub(super) fn per_pool(&self, lifetime: Lifetime) -> (usize, usize) {
        let n = self.stripe_count();
        let split = |total: usize| {
            let stripe = total.div_ceil(n);
            let invite = self.invite_share(stripe);
            match lifetime {
                Lifetime::InviteTransaction => invite.max(1),
                Lifetime::Retransmission => stripe.saturating_sub(invite).max(1),
            }
        };
        (split(self.max_entries), split(self.max_bytes))
    }

    /// The INVITE-transaction pool's part of `n`.
    fn invite_share(&self, n: usize) -> usize {
        let whole = usize::try_from(entry_ms_per_call(self)).unwrap_or(usize::MAX);
        let part = usize::try_from(self.invite_ms_per_call()).unwrap_or(0);
        (n / whole).saturating_mul(part).saturating_add((n % whole).saturating_mul(part) / whole)
    }
}

/// The new calls a second whose entries `bounds` holds with a quarter of
/// headroom, each call holding [`entry_ms_per_call`] at the bounds' mean
/// ring, every entry taken at [`TYPICAL_ENTRY_BYTES`].
pub fn calls_per_second(bounds: &MemoBounds) -> usize {
    let entries = bounds.max_entries.min(bounds.max_bytes / TYPICAL_ENTRY_BYTES);
    let call_seconds =
        usize::try_from(entry_ms_per_call(bounds) / 1_000).unwrap_or(usize::MAX).max(1);
    entries.saturating_mul(4) / call_seconds.saturating_mul(5)
}
