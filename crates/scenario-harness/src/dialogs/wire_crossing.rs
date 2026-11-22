//! What the wire record shows of a request that reaches a dialog the
//! receiving UA's own BYE ended: whether its sender sent it, as a first copy
//! the network lost, before that BYE reached the sender (RFC 3261 §15.1.2,
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
/// first copy its sender sent before that BYE arrived at it.
#[derive(Clone)]
pub struct SentBeforeBye(Arc<dyn Fn(&SipRequest, u32) -> bool + Send + Sync>);

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

/// The recorded sends and arrivals, in record order (`seq`): the first
/// `SendCalled` of `request` (its Call-ID, CSeq, method and top-Via branch)
/// is lost, its `SendLost` recorded on its lane before that lane's next
/// send; and that lane's first arrival (`RecvItem`) of the UA's BYE
/// numbered `bye_cseq` on the dialog, if any, comes after it.
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
    let bye_arrived_first = events[..at].iter().any(|s| {
        matches!(&s.event, SignalingNetworkEvent::RecvItem { bind_key, packet, .. }
            if bind_key == lane && is_the_bye(&sniff::identity(&packet.raw)))
    });
    lost && !bye_arrived_first
}

#[cfg(test)]
mod tests;
