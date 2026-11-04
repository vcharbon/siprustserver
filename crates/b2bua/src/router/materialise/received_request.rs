//! [`rebuild_received_request`]: the request a record's received half names
//! — a relay's originator's, or a held request — rebuilt from the record.

use call::RequestRecord;
use sip_message::draft::RequestDraft;
use sip_message::header::{self, HeaderName, HeaderValue, MaxForwards};
use sip_message::{Method, SipRequest, SipStr};

/// The request `record` received, as its originator sent it as far as a
/// response to it reads it: the Via stack, From, To, Call-ID and `CSeq:
/// <inbound_cseq> <method>` it arrived with, and its `Timestamp` (RFC 3261
/// §8.2.6.2, §8.2.6.1). The Request-URI is the To URI, which no response
/// reads; the headers ride as the unparsed lines the record holds. `None`
/// when the record's To does not read.
pub(super) fn rebuild_received_request(record: &RequestRecord) -> Option<SipRequest> {
    let to = header::To::parse(&SipStr::owned(&record.source_to)).ok()?;
    let method = Method::from_wire(&record.method);
    let cseq = format!("{} {}", record.inbound_cseq, method.as_str());
    let mut draft = RequestDraft::new(method, to.uri().clone());
    for via in &record.source_vias {
        draft = draft.push_raw(HeaderName::Via, SipStr::owned(via));
    }
    draft = draft
        .push_raw(HeaderName::From, SipStr::owned(&record.source_from))
        .push_raw(HeaderName::To, SipStr::owned(&record.source_to))
        .push_raw(HeaderName::CallId, SipStr::owned(&record.source_call_id))
        .push_raw(HeaderName::CSeq, SipStr::owned(&cseq))
        .push(MaxForwards::DEFAULT);
    if let Some(stamp) = &record.source_timestamp {
        draft = draft.push_raw(HeaderName::Timestamp, SipStr::owned(stamp));
    }
    draft.freeze().ok()
}
