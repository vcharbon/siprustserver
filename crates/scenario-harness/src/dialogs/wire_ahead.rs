//! What the wire record shows of a request a UA sent on an early dialog a
//! non-2xx final then ended: whether a copy of it reached the peer before
//! that final took effect there, the UA's own final arriving at the peer or
//! the peer sending its own (RFC 3261 §12.3, §17.1.2.2). The UA's table knows
//! only the first copy it sent and its own send order, so on a run that
//! declares a loss model, whose fabric loses and holds copies, it reads the
//! fact from the run's recording, and on a load call from the call's
//! [`Departures`].

use std::net::SocketAddr;
use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff::{self, Identity};
use sip_message::SipResponse;
use sip_net::RecvDisposition;
use sip_net_testkit::SignalingNetworkEvent;

use super::departures::Departures;
use super::wire_crossing::{DialogEnd, Ending};

/// Whether the request a 2xx answers reached its receiver before the final
/// that ended its dialog took effect there; `None` when the record shows no
/// copy of it delivered.
#[derive(Clone)]
pub struct AheadOfFinal(Arc<Ahead>);

/// The fact of a 2xx and the end of its dialog.
type Ahead = dyn Fn(&SipResponse, DialogEnd) -> Option<bool> + Send + Sync;

impl AheadOfFinal {
    /// The fact as `ahead` states it.
    pub fn new(
        ahead: impl Fn(&SipResponse, DialogEnd) -> Option<bool> + Send + Sync + 'static,
    ) -> Self {
        AheadOfFinal(Arc::new(ahead))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |answer, end| reached_ahead(&channel.snapshot(), answer, end))
    }

    /// The fact as a load call's departure log `log` shows it at each call:
    /// for the UA's own final only ([`Departures`]).
    pub fn from_departures(log: Departures) -> Self {
        Self::new(move |answer, end| log.ahead(answer, end))
    }

    /// Whether the request `answer` answers reached its receiver before the
    /// final `end` of its dialog took effect there; `None` when the record
    /// shows no copy of it delivered, or `end` is no final.
    pub fn holds(&self, answer: &SipResponse, end: DialogEnd) -> Option<bool> {
        (self.0)(answer, end)
    }
}

impl std::fmt::Debug for AheadOfFinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AheadOfFinal")
    }
}

/// The recorded events, in record order: the lane the first delivered copy
/// of the request `answer` answers arrived on (its Call-ID, CSeq, method and
/// top-Via branch: the UA's own copy, not one a hop relayed) is the
/// receiver's, and that copy arrived before the final took effect there: the
/// first delivered copy of the UA's own final arriving on that lane from the
/// UA's address, or that lane first sending the peer's own final to it, lost
/// or not. Another UA's final to an INVITE of the same Call-ID and number (a
/// fork's) is no end of this dialog. An arrival is its `RecvItem` at
/// delivery, so a copy the fabric held arrives when it lands.
fn reached_ahead(
    events: &[Stamped<SignalingNetworkEvent>],
    answer: &SipResponse,
    end: DialogEnd,
) -> Option<bool> {
    let Ending::Final { invite_cseq, status } = end.ending else { return None };
    let call_id = answer.call_id().as_str();
    let cseq = answer.cseq().seq();
    let method = answer.cseq().method().as_str();
    let branch = answer.top_via().branch();
    let is_request = |id: &Identity<'_>| {
        id.method == Some(method)
            && id.call_id == Some(call_id)
            && id.cseq == Some(cseq)
            && id.branch == branch
    };
    let is_final = |id: &Identity<'_>| {
        id.status == Some(status)
            && id.cseq_method == Some("INVITE")
            && id.cseq == Some(invite_cseq)
            && id.call_id == Some(call_id)
    };
    let (arrived_at, lane, ua) = events.iter().enumerate().find_map(|(i, s)| {
        arrival(s).filter(|(.., id)| is_request(id)).map(|(lane, src, _)| (i, lane, src))
    })?;
    let in_effect = events.iter().position(|s| match (&s.event, end.by_peer) {
        (SignalingNetworkEvent::SendCalled { bind_key, to, msg }, true) => {
            bind_key == lane && *to == ua && is_final(&sniff::identity(msg))
        }
        (_, false) => {
            arrival(s).is_some_and(|(at, src, id)| at == lane && src == ua && is_final(&id))
        }
        _ => false,
    });
    Some(in_effect.is_none_or(|at| arrived_at < at))
}

/// The lane, the source address and the identity of `s` when it is a
/// datagram delivered into its endpoint's inbox.
fn arrival(s: &Stamped<SignalingNetworkEvent>) -> Option<(&str, SocketAddr, Identity<'_>)> {
    match &s.event {
        SignalingNetworkEvent::RecvItem {
            bind_key,
            packet,
            disposition: RecvDisposition::Delivered,
            ..
        } => Some((bind_key.as_str(), packet.src, sniff::identity(&packet.raw))),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
