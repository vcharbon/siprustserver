//! What the wire record shows of a request that reaches a dialog the
//! receiving UA's own BYE ended: whether its sender sent it, as a first copy
//! the network lost, no later than it received that BYE (RFC 3261 §15.1.2,
//! §17.1.2.2). The fact lives on the sender's lane only, so the receiving
//! UA's dialog table reads it from the run's recording.

use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff::{self, Identity};
use sip_message::SipRequest;
use sip_net_testkit::SignalingNetworkEvent;

use crate::agent::top_via_branch;

/// Whether a request, received on a dialog the UA's BYE numbered `bye_cseq`
/// ended, is a retransmission whose first copy the network lost, and whose
/// first copy its sender sent no later than it received that BYE.
#[derive(Clone)]
pub struct SentBeforeBye(Arc<Holds>);

/// The fact of a request and the CSeq of the UA's BYE.
type Holds = dyn Fn(&SipRequest, u32) -> bool + Send + Sync;

impl SentBeforeBye {
    /// The fact as `holds` states it.
    pub fn new(holds: impl Fn(&SipRequest, u32) -> bool + Send + Sync + 'static) -> Self {
        SentBeforeBye(Arc::new(holds))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |request, bye_cseq| sent_before_bye(&channel.snapshot(), request, bye_cseq))
    }

    /// Whether it holds of `request` and the UA's BYE numbered `bye_cseq`.
    pub fn holds(&self, request: &SipRequest, bye_cseq: u32) -> bool {
        (self.0)(request, bye_cseq)
    }
}

impl std::fmt::Debug for SentBeforeBye {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SentBeforeBye")
    }
}

/// The recorded events, in record order: the first `SendCalled` of
/// `request` (its Call-ID, CSeq, method and top-Via branch) is lost, its
/// `SendLost` recorded on its lane before that lane's next send; and that
/// lane took the UA's BYE numbered `bye_cseq` from its socket
/// (`RecvConsumed`) at no instant strictly earlier than that send: a BYE
/// that arrived but was not read yet is not received, and one read at the
/// same instant is not ordered against the send (two lanes' events of one
/// instant have no order), so it crossed.
fn sent_before_bye(
    events: &[Stamped<SignalingNetworkEvent>],
    request: &SipRequest,
    bye_cseq: u32,
) -> bool {
    let branch = top_via_branch(request);
    let is_request = |id: &Identity<'_>| {
        id.method == Some(request.method().as_str())
            && id.call_id == Some(request.call_id().as_str())
            && id.cseq == Some(request.cseq().seq())
            && id.branch == branch.as_deref()
    };
    let is_the_bye = |id: &Identity<'_>| {
        id.method == Some("BYE")
            && id.call_id == Some(request.call_id().as_str())
            && id.cseq == Some(bye_cseq)
            && id.to_tag == request.from().tag()
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
    let sent_at_ms = events[at].at_ms;
    let bye_received_first = events.iter().any(|s| {
        s.at_ms < sent_at_ms
            && matches!(&s.event, SignalingNetworkEvent::RecvConsumed { bind_key, packet }
            if bind_key == lane && is_the_bye(&sniff::identity(&packet.raw)))
    });
    lost && !bye_received_first
}

#[cfg(test)]
mod tests;
