//! What a call watches on each of its mux endpoints, at the two points every
//! datagram of an endpoint crosses (its send, before simulated loss, and its
//! inbound delivery, after it), and at its retransmit engine's re-sends: SIP
//! round-trip timing, the call's message tally, and the call's wire tap,
//! which sees each datagram with the side of the call it crossed. Each of
//! them reads the datagram's one sniff ([`Datagram`]).

use std::sync::{Arc, Mutex};

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
/// transmissions and the engine's re-sends alike, with the side of the call,
/// its direction of travel (`Out`: it left the endpoint, `In`: it was on its
/// way to it), and the responses the mux delivered to the endpoint before the
/// drop, each distinct `(status, CSeq method)` once, in arrival order.
pub type LossTap = Arc<dyn Fn(Side<'_>, Dir, &Datagram<'_>, &[(u16, String)]) + Send + Sync>;

/// A call's departure tap: every datagram its endpoints put on the wire, as
/// it leaves: first transmissions, the copies the loss model holds when their
/// hold ends, and the engine's re-sends; one the loss model drops never
/// leaves. A peer's link delivers them in this order.
pub type DepartureTap = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// The responses the mux delivered to one endpoint, each distinct
/// `(status, CSeq method)` once, in arrival order: the order the network
/// delivered them in, which the endpoint's agent reads them after.
#[derive(Default)]
pub(in crate::mux) struct Arrived(Mutex<Vec<(u16, String)>>);

impl Arrived {
    fn on_inbound(&self, id: &Identity<'_>) {
        let (Some(status), Some(method)) = (id.status, id.cseq_method) else { return };
        let mut arrived = self.0.lock().unwrap();
        if !arrived.iter().any(|(s, m)| *s == status && m == method) {
            arrived.push((status, method.to_string()));
        }
    }

    fn snapshot(&self) -> Vec<(u16, String)> {
        self.0.lock().unwrap().clone()
    }
}

/// A loss tap that notes each dropped datagram in `notes`.
pub fn notes_tap(notes: LossNotes) -> LossTap {
    Arc::new(move |side, dir, d, arrived_before| {
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
            reliable: id.reliable,
            to_tag: id.to_tag.unwrap_or_default().to_string(),
            call_id: id.call_id.unwrap_or_default().to_string(),
            from_tag: from_tag(d.raw()),
            cseq: id.cseq.unwrap_or_default(),
            arrived_before: arrived_before.to_vec(),
        });
    })
}

/// One endpoint's taps.
pub(in crate::mux) struct Taps {
    rtt: Option<RttTracker>,
    tally: Option<Arc<CallTally>>,
    wire: Option<WireTap>,
    loss: Option<LossTap>,
    departure: Option<DepartureTap>,
    /// What the loss tap is told was delivered before each drop.
    arrived: Arrived,
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
        departure: Option<DepartureTap>,
        leg: Option<&str>,
    ) -> Option<Arc<Self>> {
        let watched = [rtt.is_some(), tally.is_some(), wire.is_some(), loss.is_some()];
        if !watched.contains(&true) && departure.is_none() {
            return None;
        }
        let rtt = rtt.map(RttTracker::new);
        let leg = leg.map(str::to_string);
        let arrived = Arrived::default();
        Some(Arc::new(Self { rtt, tally, wire, loss, departure, arrived, leg }))
    }

    /// The datagram `bytes` left the endpoint for the wire.
    pub(in crate::mux) fn on_departed(&self, bytes: &[u8]) {
        if let Some(departure) = &self.departure {
            departure(bytes);
        }
    }

    /// The loss model dropped `d` travelling `dir`.
    pub(in crate::mux) fn on_lost(&self, dir: Dir, d: &Datagram<'_>) {
        if let Some(loss) = &self.loss {
            loss(self.side(), dir, d, &self.arrived.snapshot());
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
        if self.loss.is_some() {
            self.arrived.on_inbound(d.id());
        }
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
