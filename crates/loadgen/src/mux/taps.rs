//! What a call watches on each of its mux endpoints, at the two points every
//! datagram of an endpoint crosses (its send, before simulated loss, and its
//! inbound delivery, after it): SIP round-trip timing, and the call's wire
//! tap, which sees each datagram with the side of the call it crossed.

use std::sync::Arc;

use tokio::time::Instant;

use super::rtt::{RttSink, RttTracker};

/// The endpoint of a call a datagram crossed: its caller, or the callee leg
/// bound under this role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side<'a> {
    Caller,
    Leg(&'a str),
}

/// Whether the endpoint sent the datagram or had it delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
}

/// A call's wire tap: every datagram its endpoints send (first transmissions
/// by the call, not the engine's retransmissions) and every datagram delivered
/// to them, duplicates included.
pub type WireTap = Arc<dyn Fn(Side<'_>, Dir, &[u8]) + Send + Sync>;

/// One endpoint's taps.
pub(in crate::mux) struct Taps {
    rtt: Option<RttTracker>,
    wire: Option<WireTap>,
    /// The callee leg's role; `None` on the caller.
    leg: Option<String>,
}

impl Taps {
    /// The taps of an endpoint (`leg`: its callee role, `None` for the caller);
    /// `None` when the call watches nothing.
    pub(in crate::mux) fn new(
        rtt: Option<RttSink>,
        wire: Option<WireTap>,
        leg: Option<&str>,
    ) -> Option<Arc<Self>> {
        if rtt.is_none() && wire.is_none() {
            return None;
        }
        let rtt = rtt.map(RttTracker::new);
        Some(Arc::new(Self { rtt, wire, leg: leg.map(str::to_string) }))
    }

    fn side(&self) -> Side<'_> {
        self.leg.as_deref().map_or(Side::Caller, Side::Leg)
    }

    pub(in crate::mux) fn on_outbound(&self, raw: &[u8], now: Instant) {
        if let Some(rtt) = &self.rtt {
            rtt.on_outbound(raw, now);
        }
        if let Some(wire) = &self.wire {
            wire(self.side(), Dir::Out, raw);
        }
    }

    pub(in crate::mux) fn on_inbound(&self, raw: &[u8], now: Instant) {
        if let Some(rtt) = &self.rtt {
            rtt.on_inbound(raw, now);
        }
        if let Some(wire) = &self.wire {
            wire(self.side(), Dir::In, raw);
        }
    }
}
