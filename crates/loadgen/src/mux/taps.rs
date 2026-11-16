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
use scenario_harness::realcall::{LossNotes, LostMessage};
use sip_message::sniff::{from_tag, session_description, Identity};

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

/// A call's loss tap: every datagram the call's loss model drops, first
/// transmissions and the engine's re-sends alike, with the side of the call
/// and its direction of travel (`Out`: it left the endpoint, `In`: it was on
/// its way to it).
pub type LossTap = Arc<dyn Fn(Side<'_>, Dir, &Datagram<'_>) + Send + Sync>;

/// A loss tap that notes each dropped datagram in `notes`.
pub fn notes_tap(notes: LossNotes) -> LossTap {
    Arc::new(move |side, dir, d| {
        let id = d.id();
        notes.note(LostMessage {
            leg: match side {
                Side::Caller => None,
                Side::Leg(role) => Some(role.to_string()),
            },
            inbound: dir == Dir::In,
            method: id.method.map(str::to_string),
            status: id.status,
            cseq_method: id.cseq_method.unwrap_or_default().to_string(),
            sdp: session_description(d.raw()).is_some(),
            call_id: id.call_id.unwrap_or_default().to_string(),
            from_tag: from_tag(d.raw()),
            cseq: id.cseq.unwrap_or_default(),
        });
    })
}

/// One endpoint's taps.
pub(in crate::mux) struct Taps {
    rtt: Option<RttTracker>,
    tally: Option<Arc<CallTally>>,
    wire: Option<WireTap>,
    loss: Option<LossTap>,
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
        loss: Option<LossTap>,
        leg: Option<&str>,
    ) -> Option<Arc<Self>> {
        if rtt.is_none() && tally.is_none() && wire.is_none() && loss.is_none() {
            return None;
        }
        let rtt = rtt.map(RttTracker::new);
        Some(Arc::new(Self { rtt, tally, wire, loss, leg: leg.map(str::to_string) }))
    }

    /// The loss model dropped `d` travelling `dir`.
    pub(in crate::mux) fn on_lost(&self, dir: Dir, d: &Datagram<'_>) {
        if let Some(loss) = &self.loss {
            loss(self.side(), dir, d);
        }
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

    /// Where the re-sends of the datagram `id` this endpoint sends are
    /// counted; `None` when the call tallies nothing.
    pub(in crate::mux) fn resend_count(&self, id: &Identity<'_>) -> Option<ResendCount> {
        let tally = self.tally.clone()?;
        Some(ResendCount { key: CallTally::resend_key(self.leg.as_deref(), id), tally })
    }
}

/// Where the re-sends of one datagram an endpoint sent are counted: the
/// tally key resolved from the datagram's sniff when it was first sent, so a
/// re-send is not sniffed again.
#[derive(Clone)]
pub(in crate::mux) struct ResendCount {
    tally: Arc<CallTally>,
    key: u64,
}

impl ResendCount {
    /// One re-send of the datagram.
    pub(in crate::mux) fn record(&self) {
        self.tally.on_resend(self.key);
    }
}
