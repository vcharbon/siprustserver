//! The reliable provisionals a UA sent (RFC 3262 §3), by the dialog each
//! opened or rode: what a PRACK's `RAck` may name.

use std::collections::BTreeSet;

use sip_message::header;
use sip_message::{SipRequest, SipResponse};

/// A provisional's dialog (Call-ID, the UA's tag, the peer's) and its `RSeq`
/// and INVITE CSeq.
type Sent = (String, String, String, u32, u32);

/// The reliable provisionals one UA sent.
#[derive(Debug, Default)]
pub(super) struct ReliableSent(BTreeSet<Sent>);

impl ReliableSent {
    /// Note `response`, sent by the UA, when it is a reliable provisional to
    /// an INVITE.
    pub(super) fn note(&mut self, response: &SipResponse) {
        let reliable = (101..200).contains(&response.status())
            && response.cseq().method().as_str() == "INVITE";
        let rseq = response.header::<header::RSeq>().and_then(Result::ok);
        let (Some(rseq), Some(local), Some(remote)) =
            (rseq.filter(|_| reliable), response.to().tag(), response.from().tag())
        else {
            return;
        };
        let call_id = response.call_id().to_string();
        let cseq = response.cseq().seq();
        self.0.insert((call_id, local.into(), remote.into(), rseq.value(), cseq));
    }

    /// Whether `prack`'s `RAck` names a reliable provisional the UA sent on
    /// the dialog the PRACK rides: such a PRACK is owed a 2xx (RFC 3262 §3).
    pub(super) fn names(&self, prack: &SipRequest) -> bool {
        let rack = prack.header::<header::RAck>().and_then(Result::ok);
        let (Some(rack), Some(local), Some(remote)) = (rack, prack.to().tag(), prack.from().tag())
        else {
            return false;
        };
        let key =
            (prack.call_id().to_string(), local.into(), remote.into(), rack.rseq(), rack.seq());
        rack.method().as_str() == "INVITE" && self.0.contains(&key)
    }
}
