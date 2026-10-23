//! The wire messages that went nowhere: outbound ones with no IP-literal next
//! hop (`b2bua_outbound_unaddressable_total`) and inbound ones or own events
//! that named no call (`b2bua_unroutable_*`).

use super::B2buaMetrics;

impl B2buaMetrics {
    /// Count one outbound message left unsent for a next hop that is not an IP
    /// literal, for `b2bua_outbound_unaddressable_total{kind}`: `kind` is
    /// `request`, `response` or `datagram`.
    pub fn record_outbound_unaddressable(&self, kind: &str) {
        self.inner.rows.outbound_unaddressable.add(&[kind], 1);
    }

    /// The count of one `b2bua_outbound_unaddressable_total{kind}` row.
    pub fn outbound_unaddressable_of(&self, kind: &str) -> u64 {
        self.inner.rows.outbound_unaddressable.get(&[kind])
    }

    /// Count one wire message naming no call that drew no answer, for
    /// `b2bua_unroutable_dropped_total{kind}`: `kind` is `ACK` or a response's
    /// status class (`1xx`…`6xx`).
    pub fn record_unroutable_dropped(&self, kind: &str) {
        self.inner.rows.unroutable_dropped.add(&[kind], 1);
    }

    /// Wire messages naming no call that drew no answer, every kind.
    pub fn unroutable_dropped_total(&self) -> u64 {
        self.inner.rows.unroutable_dropped.sum()
    }

    /// Wire messages naming no call that drew no answer, of one `kind`.
    pub fn unroutable_dropped_of(&self, kind: &str) -> u64 {
        self.inner.rows.unroutable_dropped.get(&[kind])
    }

    /// Count one wire request naming no call that this node answered `code`,
    /// for `b2bua_unroutable_refused_total{method,code}`.
    pub fn record_unroutable_refused(&self, method: &str, code: u16) {
        self.inner.rows.unroutable_refused.add(&[method, &code.to_string()], 1);
    }

    /// Wire requests naming no call answered `code`, of one `method`.
    pub fn unroutable_refused_of(&self, method: &str, code: u16) -> u64 {
        self.inner.rows.unroutable_refused.get(&[method, &code.to_string()])
    }

    /// Wire requests naming no call that this node answered, every kind.
    pub fn unroutable_refused_total(&self) -> u64 {
        self.inner.rows.unroutable_refused.sum()
    }

    /// Count one of this node's own events that named no call, for
    /// `b2bua_unroutable_internal_total{event,method}`; `method` is the timed-out
    /// request's, empty for an event that carries none.
    pub fn record_unroutable_internal(&self, event: &str, method: &str) {
        self.inner.rows.unroutable_internal.add(&[event, method], 1);
    }

    /// This node's own events that named no call, of one `event` and `method`.
    pub fn unroutable_internal_of(&self, event: &str, method: &str) -> u64 {
        self.inner.rows.unroutable_internal.get(&[event, method])
    }

    /// This node's own events that named no call, every kind.
    pub fn unroutable_internal_total(&self) -> u64 {
        self.inner.rows.unroutable_internal.sum()
    }
}
