//! [`rebuild_relayed_invite`]: the originator's INVITE a pending relay
//! snapshot names, rebuilt from the snapshot.

use call::PendingRequest;
use sip_message::draft::RequestDraft;
use sip_message::header::{self, HeaderName, HeaderValue, MaxForwards};
use sip_message::{Method, SipRequest, SipStr};

/// The INVITE `pending` relays, as its originator sent it as far as a
/// response to it reads it: the Via stack, From, To, Call-ID and `CSeq:
/// <inbound_cseq> INVITE` it arrived with, and its `Timestamp` (RFC 3261
/// §8.2.6.2, §8.2.6.1). The Request-URI is the To URI, which no response
/// reads; the headers ride as the unparsed lines the snapshot holds.
/// `None` when the snapshot's To does not read.
pub(super) fn rebuild_relayed_invite(pending: &PendingRequest) -> Option<SipRequest> {
    let to = header::To::parse(&SipStr::owned(&pending.source_to)).ok()?;
    let mut draft = RequestDraft::new(Method::Invite, to.uri().clone());
    for via in &pending.source_vias {
        draft = draft.push_raw(HeaderName::Via, SipStr::owned(via));
    }
    draft = draft
        .push_raw(HeaderName::From, SipStr::owned(&pending.source_from))
        .push_raw(HeaderName::To, SipStr::owned(&pending.source_to))
        .push_raw(HeaderName::CallId, SipStr::owned(&pending.source_call_id))
        .push_raw(HeaderName::CSeq, SipStr::owned(&format!("{} INVITE", pending.inbound_cseq)))
        .push(MaxForwards::DEFAULT);
    if let Some(stamp) = &pending.source_timestamp {
        draft = draft.push_raw(HeaderName::Timestamp, SipStr::owned(stamp));
    }
    draft.freeze().ok()
}
