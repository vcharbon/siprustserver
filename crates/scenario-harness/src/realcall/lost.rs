//! The datagrams of one call a simulated loss model dropped, as the call's
//! parties would have exchanged them: the evidence a goal or an ending reads
//! when a message it waits for never arrives ([`LossNotes`]).

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
    /// The Call-ID, From tag and CSeq number: what names a request in its
    /// dialog.
    pub call_id: String,
    pub from_tag: String,
    pub cseq: u32,
}

impl LostMessage {
    /// A response to an INVITE, its status in `range`, in either direction.
    pub fn is_invite_response_in(&self, range: std::ops::Range<u16>) -> bool {
        self.cseq_method == "INVITE" && self.status.is_some_and(|s| range.contains(&s))
    }
}

type Sink = Box<dyn Fn(&LostMessage) + Send + Sync>;

#[derive(Default)]
struct Inner {
    notes: Vec<LostMessage>,
    sink: Option<Sink>,
}

/// The call's lost datagrams, noted by the loss model as it drops them; a
/// clone notes into the same list.
#[derive(Clone, Default)]
pub struct LossNotes(Arc<Mutex<Inner>>);

impl LossNotes {
    /// Note one dropped datagram, handing it to the watching sink first.
    pub fn note(&self, lost: LostMessage) {
        let mut g = self.0.lock().unwrap();
        if let Some(sink) = &g.sink {
            sink(&lost);
        }
        g.notes.push(lost);
    }

    /// Every datagram noted so far, in the order dropped.
    pub fn all(&self) -> Vec<LostMessage> {
        self.0.lock().unwrap().notes.clone()
    }

    /// Every datagram noted so far with the party it was to or from: its
    /// callee leg, else `caller`.
    pub fn by_party(&self, caller: &str) -> Vec<(String, LostMessage)> {
        let party = |m: &LostMessage| m.leg.clone().unwrap_or_else(|| caller.to_string());
        self.all().into_iter().map(|m| (party(&m), m)).collect()
    }

    /// Hand `sink` every datagram noted so far, then each one as it is
    /// noted; replaces an earlier sink. The sink must not note.
    pub fn watch(&self, sink: impl Fn(&LostMessage) + Send + Sync + 'static) {
        let mut g = self.0.lock().unwrap();
        g.notes.iter().for_each(&sink);
        g.sink = Some(Box::new(sink));
    }

    /// Stop handing notes to the sink.
    pub fn unwatch(&self) {
        self.0.lock().unwrap().sink = None;
    }
}
