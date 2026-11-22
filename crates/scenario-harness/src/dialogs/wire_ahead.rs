//! What the wire record shows of a request a UA sent on an early dialog its
//! own non-2xx final then ended: whether a copy of it reached the peer ahead
//! of that final (RFC 3261 §12.3, §17.1.2.2). The UA's table knows only the
//! first copy it sent, so on a run that declares a loss model, where that copy
//! may be lost and a retransmission taken instead, it reads the fact from the
//! run's recording.

use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff::{self, Identity};
use sip_message::SipResponse;
use sip_net_testkit::SignalingNetworkEvent;

use super::wire_loss::send_lost;

/// Whether the request a 2xx answers reached its receiver ahead of the
/// sender's own non-2xx final to the INVITE numbered `invite_cseq`.
#[derive(Clone)]
pub struct AheadOfFinal(Arc<Ahead>);

/// The fact of a 2xx, the INVITE's number and the final's status.
type Ahead = dyn Fn(&SipResponse, u32, u16) -> bool + Send + Sync;

impl AheadOfFinal {
    /// The fact as `ahead` states it.
    pub fn new(ahead: impl Fn(&SipResponse, u32, u16) -> bool + Send + Sync + 'static) -> Self {
        AheadOfFinal(Arc::new(ahead))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |answer, invite_cseq, status| {
            reached_ahead(&channel.snapshot(), answer, invite_cseq, status)
        })
    }

    /// Whether the request `answer` answers reached its receiver ahead of the
    /// `status` final its sender sent to the INVITE numbered `invite_cseq`.
    pub fn holds(&self, answer: &SipResponse, invite_cseq: u32, status: u16) -> bool {
        (self.0)(answer, invite_cseq, status)
    }
}

impl std::fmt::Debug for AheadOfFinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AheadOfFinal")
    }
}

/// The recorded events, in record order: the lane of the first `SendCalled`
/// of the request `answer` answers (its Call-ID, CSeq, method and top-Via
/// branch) sends a copy of it the network delivers before that lane's first
/// delivered copy of the final. A lane's datagrams reach their receiver in
/// the order it sent them, so that copy was taken while the dialog lived.
fn reached_ahead(
    events: &[Stamped<SignalingNetworkEvent>],
    answer: &SipResponse,
    invite_cseq: u32,
    status: u16,
) -> bool {
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
    let sent = |i: usize| match &events[i].event {
        SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
            Some((bind_key, sniff::identity(msg)))
        }
        _ => None,
    };
    let Some(lane) =
        (0..events.len()).filter_map(sent).find(|(_, id)| is_request(id)).map(|(lane, _)| lane)
    else {
        return false;
    };
    let delivered = |i: usize, of: &dyn Fn(&Identity<'_>) -> bool| {
        sent(i).is_some_and(|(at, id)| at == lane && of(&id)) && !send_lost(events, i)
    };
    let final_at = (0..events.len()).find(|&i| delivered(i, &is_final)).unwrap_or(events.len());
    (0..final_at).any(|i| delivered(i, &is_request))
}

#[cfg(test)]
mod tests;
