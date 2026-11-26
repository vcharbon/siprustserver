//! The CDR sink's counts: each record's end, written or dropped by reason,
//! and a broker sink's link (`b2bua_cdr_*`).

use std::sync::atomic::{AtomicU64, Ordering};

use super::catalogue::cdr::{
    BROKER_CONNECTED, BROKER_CONNECTS, BROKER_SESSION_ENDS, CONNECT_OUTCOME, DROPPED, DROP_REASON,
    SESSION_END, WRITTEN,
};
use super::cells::{add, add_at, get, get_at};
use super::B2buaMetrics;
use crate::cdr::{BrokerConnect, BrokerSessionEnd, CdrDrop};

/// The CDR sink's cells, each indexed by its enum's declaration order.
#[derive(Debug, Default)]
pub(super) struct CdrCounts {
    written: AtomicU64,
    dropped: [AtomicU64; CdrDrop::ALL.len()],
    /// Sessions opened and not ended yet: each open adds one, its first end
    /// takes it back.
    sessions_live: AtomicU64,
    connects: [AtomicU64; BrokerConnect::ALL.len()],
    session_ends: [AtomicU64; BrokerSessionEnd::ALL.len()],
}

impl CdrCounts {
    /// Append the CDR families, every declared label set included.
    pub(super) fn render(&self, s: &mut String) {
        WRITTEN.render_value(s, get(&self.written));
        DROPPED.render(s, |series| get_at(&self.dropped, series.index(&DROP_REASON)));
        BROKER_CONNECTED.render_value(s, get(&self.sessions_live));
        BROKER_CONNECTS.render(s, |series| get_at(&self.connects, series.index(&CONNECT_OUTCOME)));
        BROKER_SESSION_ENDS
            .render(s, |series| get_at(&self.session_ends, series.index(&SESSION_END)));
    }
}

impl B2buaMetrics {
    /// One record the sink delivered.
    pub fn bump_cdr_written(&self) {
        add(&self.inner.cdr.written, 1);
    }

    /// Records the sink delivered.
    pub fn cdr_written_total(&self) -> u64 {
        get(&self.inner.cdr.written)
    }

    /// One record that ended undelivered, for `why`.
    pub fn bump_cdr_dropped(&self, why: CdrDrop) {
        add_at(&self.inner.cdr.dropped, why as usize, 1);
    }

    /// Records that ended undelivered, every reason.
    pub fn cdr_dropped_total(&self) -> u64 {
        self.inner.cdr.dropped.iter().map(get).sum()
    }

    /// Records that ended undelivered for `why`.
    pub fn cdr_dropped_of(&self, why: CdrDrop) -> u64 {
        get_at(&self.inner.cdr.dropped, why as usize)
    }

    /// Each reason records ended undelivered for, with their count, in
    /// [`CdrDrop::ALL`] order; a reason no record ended for is left out.
    pub fn cdr_drops(&self) -> Vec<(CdrDrop, u64)> {
        CdrDrop::ALL
            .into_iter()
            .map(|why| (why, self.cdr_dropped_of(why)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }

    /// One connect attempt of the broker sink ended as `outcome`.
    pub fn bump_cdr_broker_connect(&self, outcome: BrokerConnect) {
        add_at(&self.inner.cdr.connects, outcome as usize, 1);
    }

    /// The broker sink's connect attempts that ended as `outcome`.
    pub fn cdr_broker_connects_of(&self, outcome: BrokerConnect) -> u64 {
        get_at(&self.inner.cdr.connects, outcome as usize)
    }

    /// A broker session opened. Its end is counted once, by
    /// [`Self::cdr_broker_session_ended`].
    pub fn cdr_broker_session_opened(&self) {
        add(&self.inner.cdr.sessions_live, 1);
    }

    /// A session [`Self::cdr_broker_session_opened`] counted ended, for
    /// `why`; called once per opened session.
    pub fn cdr_broker_session_ended(&self, why: BrokerSessionEnd) {
        let live = &self.inner.cdr.sessions_live;
        let _ = live.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        add_at(&self.inner.cdr.session_ends, why as usize, 1);
    }

    /// The broker sessions open now: 1 while the sink publishes through one.
    pub fn cdr_broker_connected(&self) -> u64 {
        get(&self.inner.cdr.sessions_live)
    }

    /// The broker sessions that ended for `why`.
    pub fn cdr_broker_session_ends_of(&self, why: BrokerSessionEnd) -> u64 {
        get_at(&self.inner.cdr.session_ends, why as usize)
    }
}
