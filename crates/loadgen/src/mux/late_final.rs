//! The INVITE client transaction outlives its call (RFC 3261 §17.1.1.3): a
//! caller endpoint keeps the latest INVITE of each dialog until a 2xx answers
//! it, hands what it kept to the socket registry when it drops, and a non-2xx
//! final to a held INVITE — retransmissions included — draws its ACK from the
//! mux after the call has gone.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::parser::custom::CustomParser;
use sip_message::preparse::is_invite_request_buffer;
use sip_message::sniff::{call_id, cseq_method_token, cseq_number, is_response, resp_status};
use sip_message::{serialize, SipMessage, SipParser};

/// One INVITE a caller sent: its CSeq number, its bytes and where it went.
#[derive(Clone)]
pub(in crate::mux) struct SentInvite {
    cseq: u32,
    raw: Vec<u8>,
    dst: SocketAddr,
}

/// A caller endpoint's INVITEs still open to a non-2xx final, one per dialog
/// (Call-ID): the latest sent, until a 2xx to it arrives.
#[derive(Default)]
pub(in crate::mux) struct SentInvites(Mutex<HashMap<String, SentInvite>>);

impl SentInvites {
    /// Keep `raw` when it is an INVITE request (its retransmission replaces it
    /// with the same bytes).
    pub(in crate::mux) fn on_outbound(&self, raw: &[u8], dst: SocketAddr) {
        if !is_invite_request_buffer(raw) {
            return;
        }
        let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) else { return };
        self.0.lock().unwrap().insert(cid, SentInvite { cseq, raw: raw.to_vec(), dst });
    }

    /// Forget the INVITE a 2xx answers: its ACK is the dialog's, never the
    /// transaction's.
    pub(in crate::mux) fn on_inbound(&self, raw: &[u8]) {
        if !is_response(raw) || !resp_status(raw).is_some_and(|s| (200..300).contains(&s)) {
            return;
        }
        if cseq_method_token(raw).as_deref() != Some("INVITE") {
            return;
        }
        let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) else { return };
        let mut held = self.0.lock().unwrap();
        if held.get(&cid).is_some_and(|i| i.cseq == cseq) {
            held.remove(&cid);
        }
    }

    /// The INVITE held for dialog `call_id`, removed.
    pub(in crate::mux) fn take(&self, call_id: &str) -> Option<SentInvite> {
        self.0.lock().unwrap().remove(call_id)
    }
}

/// The ACK `invite` owes `response` and where it goes (where the INVITE went):
/// `Some` only for a non-2xx final to that INVITE (same CSeq number, method
/// INVITE).
pub(in crate::mux) fn ack_for(
    invite: &SentInvite,
    response: &[u8],
) -> Option<(Vec<u8>, SocketAddr)> {
    if !is_response(response) || !resp_status(response).is_some_and(|s| s >= 300) {
        return None;
    }
    if cseq_method_token(response).as_deref() != Some("INVITE")
        || cseq_number(response) != Some(invite.cseq)
    {
        return None;
    }
    let parser = CustomParser::new();
    let Ok(SipMessage::Request(req)) = parser.parse(&invite.raw) else { return None };
    let Ok(SipMessage::Response(resp)) = parser.parse(response) else { return None };
    let ack = generate_ack_for_non_2xx(&req, &resp, &[]);
    Some((serialize(&SipMessage::Request(ack)), invite.dst))
}
