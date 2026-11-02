//! One independently locked stripe of the branch memo: a [`Pool`] per
//! [`Lifetime`], each with its own share of the stripe's bounds, so entries
//! of one lifetime never push out entries of the other. The
//! INVITE-transaction pool holds only INVITEs placed off their memo-free
//! route (ADR-0009 X7), whose entries move to the retransmission pool when
//! their final passes; the retransmission pool holds the copies' memos of
//! initial INVITEs and of requests placed afresh, and ACK hops.

use super::pool::{Change, Pool};
use super::{CancelEntry, Lifetime, MemoBounds};

pub(super) struct Stripe {
    invite: Pool,
    retransmission: Pool,
}

impl Stripe {
    pub(super) fn new(bounds: &MemoBounds) -> Self {
        let pool = |lifetime| {
            let (entries, bytes) = bounds.per_pool(lifetime);
            Pool::new(entries, bytes)
        };
        Self {
            invite: pool(Lifetime::InviteTransaction),
            retransmission: pool(Lifetime::Retransmission),
        }
    }

    fn pools(&mut self, lifetime: Lifetime) -> (&mut Pool, &mut Pool) {
        match lifetime {
            Lifetime::InviteTransaction => (&mut self.invite, &mut self.retransmission),
            Lifetime::Retransmission => (&mut self.retransmission, &mut self.invite),
        }
    }

    /// Forget the expired entries of both pools.
    pub(super) fn expire(&mut self, now_ms: u64, change: &mut Change) {
        self.invite.expire(now_ms, change);
        self.retransmission.expire(now_ms, change);
    }

    /// Write `entry` under `key` into `lifetime`'s pool, after that pool's
    /// expired entries are forgotten; the key leaves the other pool.
    pub(super) fn insert(
        &mut self,
        key: &str,
        entry: CancelEntry,
        lifetime: Lifetime,
        now_ms: u64,
        change: &mut Change,
    ) {
        let (own, other) = self.pools(lifetime);
        other.remove(key, change);
        own.expire(now_ms, change);
        own.insert(key, entry, now_ms.saturating_add(lifetime.ttl_ms()), change);
    }

    /// The live entry under `key`; one held in the INVITE-transaction pool
    /// moves to the retransmission pool first, to expire one
    /// [`Lifetime::Retransmission`] past `now_ms`.
    pub(super) fn shorten(
        &mut self,
        key: &str,
        now_ms: u64,
        change: &mut Change,
    ) -> Option<CancelEntry> {
        let Some(entry) = self.invite.get(key, now_ms, change) else {
            return self.retransmission.get(key, now_ms, change);
        };
        let lifetime = Lifetime::Retransmission;
        self.insert(key, entry.clone(), lifetime, now_ms, change);
        Some(entry)
    }

    /// Forget the entry under `key`, from whichever pool holds it.
    pub(super) fn forget(&mut self, key: &str, change: &mut Change) {
        self.invite.remove(key, change);
        self.retransmission.remove(key, change);
    }

    /// The live entry under `key`, from whichever pool holds it.
    pub(super) fn get(
        &mut self,
        key: &str,
        now_ms: u64,
        change: &mut Change,
    ) -> Option<CancelEntry> {
        self.invite
            .get(key, now_ms, change)
            .or_else(|| self.retransmission.get(key, now_ms, change))
    }
}
