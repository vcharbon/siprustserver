//! What a call watches on each of its mux endpoints, at the two points every
//! datagram of an endpoint crosses (its send, before simulated loss, and its
//! inbound delivery, after it), and at its retransmit engine's re-sends: SIP
//! round-trip timing, the call's message tally, and the call's wire tap,
//! which sees each datagram with the side of the call it crossed. Each of
//! them reads the datagram's one sniff ([`Datagram`]).

use std::sync::Arc;

use tokio::time::Instant;

use super::datagram::Datagram;
use super::rtt::{RttSink, RttTracker};
use super::tally::{CallTally, LateRepair};
use sip_message::sniff::{identity, Identity};

/// The endpoint of a call a datagram crossed: its caller, or the callee leg
/// bound under this role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side<'a> {
    Caller,
    Leg(&'a str),
}

/// Whether the endpoint sent the datagram or had it delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Out,
    In,
}

/// A call's wire tap: every datagram its endpoints send (first transmissions
/// by the call, not the engine's retransmissions) and every datagram delivered
/// to them, duplicates included.
pub type WireTap = Arc<dyn Fn(Side<'_>, Dir, &Datagram<'_>) + Send + Sync>;

/// One endpoint's taps.
pub(in crate::mux) struct Taps {
    rtt: Option<RttTracker>,
    tally: Option<Arc<CallTally>>,
    wire: Option<WireTap>,
    /// The callee leg's role; `None` on the caller.
    leg: Option<String>,
}

impl Taps {
    /// The taps of an endpoint (`leg`: its callee role, `None` for the caller);
    /// `None` when the call watches nothing.
    pub(in crate::mux) fn new(
        rtt: Option<RttSink>,
        tally: Option<Arc<CallTally>>,
        wire: Option<WireTap>,
        leg: Option<&str>,
    ) -> Option<Arc<Self>> {
        if rtt.is_none() && tally.is_none() && wire.is_none() {
            return None;
        }
        let rtt = rtt.map(RttTracker::new);
        Some(Arc::new(Self { rtt, tally, wire, leg: leg.map(str::to_string) }))
    }

    fn side(&self) -> Side<'_> {
        self.leg.as_deref().map_or(Side::Caller, Side::Leg)
    }

    pub(in crate::mux) fn on_outbound(&self, d: &Datagram<'_>, now: Instant) {
        if let Some(rtt) = &self.rtt {
            rtt.on_outbound(d.id(), now);
        }
        if let Some(tally) = &self.tally {
            tally.on_datagram(self.leg.as_deref(), Dir::Out, d.id(), now);
        }
        if let Some(wire) = &self.wire {
            wire(self.side(), Dir::Out, d);
        }
    }

    pub(in crate::mux) fn on_inbound(&self, d: &Datagram<'_>, now: Instant) {
        if let Some(rtt) = &self.rtt {
            rtt.on_inbound(d.id(), now);
        }
        if let Some(tally) = &self.tally {
            tally.on_datagram(self.leg.as_deref(), Dir::In, d.id(), now);
        }
        if let Some(wire) = &self.wire {
            wire(self.side(), Dir::In, d);
        }
    }

    /// The report of a late re-send of `answer`, a final this endpoint sent
    /// (see [`CallTally::late_repair`]).
    pub(in crate::mux) fn late_repair(&self, answer: &Identity<'_>) -> Option<LateRepair> {
        self.tally.as_ref()?.late_repair(self.leg.as_deref(), answer)
    }

    /// A re-send this endpoint's retransmit engine made.
    pub(in crate::mux) fn on_resend(&self, raw: &[u8]) {
        if let Some(tally) = &self.tally {
            tally.on_resend(self.leg.as_deref(), &identity(raw));
        }
    }
}
