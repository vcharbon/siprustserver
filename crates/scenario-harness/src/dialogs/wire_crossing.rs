//! What the wire record shows of a request that reaches a dialog already
//! ended at the receiving UA: whether it is a retransmission whose first copy
//! the network lost, that first copy sent before its sender knew the dialog
//! ended (RFC 3261 §12.2.2, §15.1.2, §17.1.2.2). The fact lives on the
//! sender's lane only, so the receiving UA's dialog table reads it from the
//! run's recording; only a run that declares a loss model hands it over.

use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff::{self, Identity};
use sip_message::SipRequest;
use sip_net_testkit::SignalingNetworkEvent;

use crate::agent::top_via_branch;

/// The message that ended a dialog at a UA, and which side sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogEnd {
    pub ending: Ending,
    /// The peer, the sender of the request judged, sent it; else the UA did.
    pub by_peer: bool,
}

/// What ended a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The transaction of the BYE numbered `cseq` (RFC 3261 §15).
    Bye { cseq: u32 },
    /// The non-2xx final `status` to the initial INVITE numbered
    /// `invite_cseq`, which ends every early dialog of that INVITE (§12.1.2).
    Final { invite_cseq: u32, status: u16 },
}

/// Whether a request, received on a dialog a [`DialogEnd`] ended, is a
/// retransmission whose first copy the network lost, and whose first copy
/// its sender sent before it knew of that end.
#[derive(Clone)]
pub struct SentBeforeEnd(Arc<Holds>);

/// The fact of a request and the end of its dialog.
type Holds = dyn Fn(&SipRequest, DialogEnd) -> bool + Send + Sync;

impl SentBeforeEnd {
    /// The fact as `holds` states it.
    pub fn new(holds: impl Fn(&SipRequest, DialogEnd) -> bool + Send + Sync + 'static) -> Self {
        SentBeforeEnd(Arc::new(holds))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |request, end| sent_before_end(&channel.snapshot(), request, end))
    }

    /// Whether it holds of `request` and the `end` of its dialog.
    pub fn holds(&self, request: &SipRequest, end: DialogEnd) -> bool {
        (self.0)(request, end)
    }
}

impl std::fmt::Debug for SentBeforeEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SentBeforeEnd")
    }
}

/// The recorded events, in record order: the first `SendCalled` of
/// `request` (its Call-ID, CSeq, method and top-Via branch) is lost, its
/// `SendLost` recorded on its lane before that lane's next send; and that
/// lane shows the `end` after it. An end the peer sent is the lane's first
/// `SendCalled` of it, after that first copy: a retransmission of the end
/// (§17.1.2.2, §17.2.1) was sent when its first copy left. An end the UA
/// sent is one the lane took from its socket (`RecvConsumed`) at no instant
/// strictly earlier than the request's first copy: an end that arrived but
/// was not read yet is not known, and one read at the same instant is not
/// ordered against the send (two lanes' events of one instant have no
/// order), so it crossed.
fn sent_before_end(
    events: &[Stamped<SignalingNetworkEvent>],
    request: &SipRequest,
    end: DialogEnd,
) -> bool {
    let branch = top_via_branch(request);
    let is_request = |id: &Identity<'_>| {
        id.method == Some(request.method().as_str())
            && id.call_id == Some(request.call_id().as_str())
            && id.cseq == Some(request.cseq().seq())
            && id.branch == branch.as_deref()
    };
    let Some((at, lane, first)) = events.iter().enumerate().find_map(|(i, s)| match &s.event {
        SignalingNetworkEvent::SendCalled { bind_key, msg, .. }
            if is_request(&sniff::identity(msg)) =>
        {
            Some((i, bind_key, msg))
        }
        _ => None,
    }) else {
        return false;
    };
    let lost = events[at + 1..]
        .iter()
        .map(|s| &s.event)
        .take_while(|e| !matches!(e, SignalingNetworkEvent::SendCalled { bind_key, .. } if bind_key == lane))
        .any(|e| matches!(e, SignalingNetworkEvent::SendLost { bind_key, msg, .. } if bind_key == lane && msg == first));
    let is_the_end = |raw: &[u8]| is_end_of(request, end, &sniff::identity(raw));
    let known_before = if end.by_peer {
        let first_end = events.iter().position(|s| {
            matches!(&s.event, SignalingNetworkEvent::SendCalled { bind_key, msg, .. }
            if bind_key == lane && is_the_end(msg))
        });
        !first_end.is_some_and(|i| i > at)
    } else {
        let sent_at_ms = events[at].at_ms;
        events.iter().any(|s| {
            s.at_ms < sent_at_ms
                && matches!(&s.event, SignalingNetworkEvent::RecvConsumed { bind_key, packet }
                if bind_key == lane && is_the_end(&packet.raw))
        })
    };
    lost && !known_before
}

/// Whether `id` is the message that is `end` of `request`'s dialog: a BYE of
/// its Call-ID and number, addressed to the request's receiver when the peer
/// sent it and to its sender when the UA did; or the final of that status to
/// the INVITE of its Call-ID and number, whatever early dialog its To-tag
/// names.
fn is_end_of(request: &SipRequest, end: DialogEnd, id: &Identity<'_>) -> bool {
    let call_id = id.call_id == Some(request.call_id().as_str());
    match end.ending {
        Ending::Bye { cseq } => {
            let addressee = if end.by_peer { request.to().tag() } else { request.from().tag() };
            call_id && id.method == Some("BYE") && id.cseq == Some(cseq) && id.to_tag == addressee
        }
        Ending::Final { invite_cseq, status } => {
            call_id
                && id.status == Some(status)
                && id.cseq_method == Some("INVITE")
                && id.cseq == Some(invite_cseq)
        }
    }
}

#[cfg(test)]
mod tests;
