//! The datagrams of one call a simulated loss model dropped, as the call's
//! parties would have exchanged them: the evidence an ending reads when a
//! message it expects never arrived ([`LossNotes`]).

use std::sync::{Arc, Mutex};

/// One datagram the loss model dropped, to or from a party of the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostMessage {
    /// The callee leg the datagram was to or from; `None`: the caller.
    pub leg: Option<String>,
    /// The datagram was on its way to the party (else it left the party).
    pub inbound: bool,
    /// The request method; `None` for a response.
    pub method: Option<String>,
    /// The response status; `None` for a request.
    pub status: Option<u16>,
    /// The CSeq method.
    pub cseq_method: String,
    /// The datagram carried a session description.
    pub sdp: bool,
}

impl LostMessage {
    /// A response to an INVITE on its way to the party, its status in `range`.
    pub fn is_invite_response_in(&self, range: std::ops::Range<u16>) -> bool {
        self.inbound
            && self.cseq_method == "INVITE"
            && self.status.is_some_and(|s| range.contains(&s))
    }
}

/// The call's lost datagrams, noted by the loss model as it drops them; a
/// clone notes into the same list.
#[derive(Debug, Clone, Default)]
pub struct LossNotes(Arc<Mutex<Vec<LostMessage>>>);

impl LossNotes {
    /// Note one dropped datagram.
    pub fn note(&self, lost: LostMessage) {
        self.0.lock().unwrap().push(lost);
    }

    /// Every datagram noted so far, in the order dropped.
    pub fn all(&self) -> Vec<LostMessage> {
        self.0.lock().unwrap().clone()
    }
}
