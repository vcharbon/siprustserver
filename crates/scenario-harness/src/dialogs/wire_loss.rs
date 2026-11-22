//! What the wire record shows of the requests the network lost on their way
//! to a UA: each copy the loss model dropped (`SendLost`), named by its
//! dialog, CSeq and method. The fact lives on the sender's lane only, so the
//! receiving UA's dialog table reads it from the run's recording; only a run
//! that declares a loss model hands it over.

use std::sync::Arc;

use layer_harness::{Channel, Stamped};
use sip_message::sniff;
use sip_net_testkit::SignalingNetworkEvent;

/// The `(CSeq, method)` of every request the network lost on the dialog a
/// Call-ID, a From-tag and a To-tag name.
#[derive(Clone)]
pub struct LostOnTheWay(Arc<Lost>);

/// The fact of a Call-ID, a From-tag and a To-tag.
type Lost = dyn Fn(&str, &str, &str) -> Vec<(u32, String)> + Send + Sync;

impl LostOnTheWay {
    /// The fact as `lost` states it.
    pub fn new(
        lost: impl Fn(&str, &str, &str) -> Vec<(u32, String)> + Send + Sync + 'static,
    ) -> Self {
        LostOnTheWay(Arc::new(lost))
    }

    /// The fact as the run's recording `channel` shows it at each call.
    pub fn from_wire(channel: Channel<SignalingNetworkEvent>) -> Self {
        Self::new(move |call_id, from_tag, to_tag| {
            lost_requests(&channel.snapshot(), call_id, from_tag, to_tag)
        })
    }

    /// The `(CSeq, method)` of every request on the dialog Call-ID
    /// `call_id`, From-tag `from_tag`, To-tag `to_tag` the network lost.
    pub fn lost(&self, call_id: &str, from_tag: &str, to_tag: &str) -> Vec<(u32, String)> {
        (self.0)(call_id, from_tag, to_tag)
    }
}

impl std::fmt::Debug for LostOnTheWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LostOnTheWay")
    }
}

/// Every `SendLost` of a request carrying `call_id`, `from_tag` and `to_tag`,
/// as its `(CSeq, method)`, in record order.
fn lost_requests(
    events: &[Stamped<SignalingNetworkEvent>],
    call_id: &str,
    from_tag: &str,
    to_tag: &str,
) -> Vec<(u32, String)> {
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
            on_dialog.then_some((id.cseq?, id.method?.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests;
