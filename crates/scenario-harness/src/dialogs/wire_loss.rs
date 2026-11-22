//! What the wire record shows of the requests the network lost on their way
//! to a UA: each copy the loss model dropped (`SendLost`), named by its
//! dialog, CSeq, method and top-Via branch. The fact lives on the sender's
//! lane only, so the receiving UA's dialog table reads it from the run's
//! recording; only a run that declares a loss model hands it over.

use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff;
use sip_net_testkit::SignalingNetworkEvent;

/// A request copy the network lost on its way to the UA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostRequest {
    pub cseq: u32,
    pub method: String,
    /// The branch of its top Via, which names its transaction (RFC 3261
    /// §17.2.3); `None` when it carries none.
    pub branch: Option<String>,
}

impl LostRequest {
    /// Whether this is a copy of the request of `branch`, numbered `cseq`,
    /// of `method`: a request with no branch is no copy of any.
    pub fn is_copy_of(&self, cseq: u32, method: &str, branch: Option<&str>) -> bool {
        self.cseq == cseq
            && self.method == method
            && branch.is_some()
            && self.branch.as_deref() == branch
    }
}

/// Every request the network lost on the dialog a Call-ID, a From-tag and a
/// To-tag name.
#[derive(Clone)]
pub struct LostOnTheWay(Arc<Lost>);

/// The fact of a Call-ID, a From-tag and a To-tag.
type Lost = dyn Fn(&str, &str, &str) -> Vec<LostRequest> + Send + Sync;

impl LostOnTheWay {
    /// The fact as `lost` states it.
    pub fn new(
        lost: impl Fn(&str, &str, &str) -> Vec<LostRequest> + Send + Sync + 'static,
    ) -> Self {
        LostOnTheWay(Arc::new(lost))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |call_id, from_tag, to_tag| {
            lost_requests(&channel.snapshot(), call_id, from_tag, to_tag)
        })
    }

    /// Every request on the dialog Call-ID `call_id`, From-tag `from_tag`,
    /// To-tag `to_tag` the network lost, in record order.
    pub fn lost(&self, call_id: &str, from_tag: &str, to_tag: &str) -> Vec<LostRequest> {
        (self.0)(call_id, from_tag, to_tag)
    }
}

impl std::fmt::Debug for LostOnTheWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LostOnTheWay")
    }
}

/// Whether the `SendCalled` at `at` in `events` was lost: its lane records
/// the `SendLost` of its bytes before that lane's next `SendCalled`.
pub(crate) fn send_lost(events: &[Stamped<SignalingNetworkEvent>], at: usize) -> bool {
    let SignalingNetworkEvent::SendCalled { bind_key: lane, msg: sent, .. } = &events[at].event
    else {
        return false;
    };
    events[at + 1..]
        .iter()
        .map(|s| &s.event)
        .take_while(|e| !matches!(e, SignalingNetworkEvent::SendCalled { bind_key, .. } if bind_key == lane))
        .any(|e| matches!(e, SignalingNetworkEvent::SendLost { bind_key, msg, .. } if bind_key == lane && msg == sent))
}

/// Every `SendLost` of a request carrying `call_id`, `from_tag` and `to_tag`,
/// in record order.
fn lost_requests(
    events: &[Stamped<SignalingNetworkEvent>],
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
) -> Vec<LostRequest> {
    events
        .iter()
        .filter_map(|s| match &s.event {
            SignalingNetworkEvent::SendLost { msg, .. } => Some(msg),
            _ => None,
        })
        .filter_map(|msg| {
            let id = sniff::identity(msg);
            let on_dialog = id.call_id == Some(call_id)
                && id.to_tag == Some(to_tag)
                && sniff::from_tag(msg) == from_tag;
            on_dialog.then_some(LostRequest {
                cseq: id.cseq?,
                method: id.method?.to_string(),
                branch: id.branch.map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
