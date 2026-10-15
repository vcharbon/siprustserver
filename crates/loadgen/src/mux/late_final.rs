//! The INVITE client transaction outlives its call (RFC 3261 §17.1.1.3,
//! §13.2.2.4): a caller endpoint holds what each dialog's latest INVITE may
//! still owe a final, hands it to the socket registry when it drops, and a
//! final arriving after the call has gone draws what it is owed from the mux:
//!
//! - an INVITE still waiting for a final is held whole ([`Held::Open`]);
//! - once a non-2xx final arrived, only the ACK it drew is held
//!   ([`Held::Completed`]), resent to each retransmission of that final;
//! - a 2xx to a held INVITE draws its ACK and the BYE that ends the dialog
//!   no call holds ([`Held::Answered`]): a retransmitted 2xx draws the ACK
//!   again, the BYE's own final is absorbed.
//!
//! A 2xx to a live call's INVITE ends the hold: its ACK is the dialog's.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::generators::{
    generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx, generate_bye_for_2xx_from_invite,
};
use sip_message::parser::custom::CustomParser;
use sip_message::preparse::is_invite_request_buffer;
use sip_message::sniff::{call_id, cseq_method_token, cseq_number, is_response, resp_status};
use sip_message::{serialize, SipMessage, SipParser, SipRequest, SipResponse};

/// What one dialog's latest INVITE may still owe a final, and where its
/// requests go (where the INVITE went).
#[derive(Clone)]
pub(in crate::mux) struct Held {
    cseq: u32,
    dst: SocketAddr,
    state: HeldState,
}

#[derive(Clone)]
enum HeldState {
    /// No final yet: the INVITE itself.
    Open(Vec<u8>),
    /// A non-2xx final arrived: the ACK it drew.
    Completed(Vec<u8>),
    /// A 2xx arrived after the call released: its ACK, and the CSeq of the
    /// BYE sent to end the dialog.
    Answered { ack: Vec<u8>, bye_cseq: u32 },
}

/// What a final arriving for a held INVITE draws.
pub(in crate::mux) enum Reply {
    /// Nothing for this hold (not its transaction): the datagram is a stray.
    NotHeld,
    /// Absorbed: the final of the BYE the hold sent.
    Absorbed,
    /// The ACK of a non-2xx final.
    AckNon2xx(Vec<u8>, SocketAddr),
    /// The ACK of a 2xx, and, the first time, the BYE ending its dialog.
    Answered(Vec<(Vec<u8>, SocketAddr)>),
}

fn parse_request(raw: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Request(r)) => Some(r),
        _ => None,
    }
}

fn parse_response(raw: &[u8]) -> Option<SipResponse> {
    match CustomParser::new().parse(raw) {
        Ok(SipMessage::Response(r)) => Some(r),
        _ => None,
    }
}

fn wire(request: SipRequest) -> Vec<u8> {
    serialize(&SipMessage::Request(request))
}

/// A fresh RFC 3261 branch for a request the hold originates.
fn branch() -> String {
    format!("z9hG4bK{}", uuid::Uuid::new_v4().simple())
}

impl Held {
    /// What the final `response` draws from this hold.
    pub(in crate::mux) fn on_final(&mut self, response: &[u8]) -> Reply {
        let (Some(status), Some(cseq), Some(method)) =
            (resp_status(response), cseq_number(response), cseq_method_token(response))
        else {
            return Reply::NotHeld;
        };
        if !is_response(response) || status < 200 {
            return Reply::NotHeld;
        }
        let (dst, held_cseq) = (self.dst, self.cseq);
        match (&self.state, method.as_str()) {
            (HeldState::Answered { bye_cseq, .. }, "BYE") if *bye_cseq == cseq => Reply::Absorbed,
            _ if method != "INVITE" || cseq != held_cseq => Reply::NotHeld,
            (HeldState::Completed(ack), _) if status >= 300 => Reply::AckNon2xx(ack.clone(), dst),
            (HeldState::Answered { ack, .. }, _) if status < 300 => {
                Reply::Answered(vec![(ack.clone(), dst)])
            }
            (HeldState::Open(invite), _) => {
                let (Some(invite), Some(resp)) = (parse_request(invite), parse_response(response))
                else {
                    return Reply::NotHeld;
                };
                if status >= 300 {
                    let ack = wire(generate_ack_for_non_2xx(&invite, &resp, &[]));
                    self.state = HeldState::Completed(ack.clone());
                    return Reply::AckNon2xx(ack, dst);
                }
                let ack = wire(generate_ack_for_2xx_from_invite(&invite, &resp, &branch()));
                let bye_cseq = held_cseq + 1;
                let bye = generate_bye_for_2xx_from_invite(&invite, &resp, bye_cseq, &branch());
                self.state = HeldState::Answered { ack: ack.clone(), bye_cseq };
                Reply::Answered(vec![(ack, dst), (wire(bye), dst)])
            }
            _ => Reply::NotHeld,
        }
    }
}

/// A caller endpoint's holds, one per dialog (Call-ID).
#[derive(Default)]
pub(in crate::mux) struct SentInvites(Mutex<HashMap<String, Held>>);

impl SentInvites {
    /// Hold `raw` when it is an INVITE request (its retransmission replaces it
    /// with the same bytes).
    pub(in crate::mux) fn on_outbound(&self, raw: &[u8], dst: SocketAddr) {
        if !is_invite_request_buffer(raw) {
            return;
        }
        let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) else { return };
        let held = Held { cseq, dst, state: HeldState::Open(raw.to_vec()) };
        self.0.lock().unwrap().insert(cid, held);
    }

    /// A final to the live call's held INVITE: a 2xx ends the hold, a non-2xx
    /// keeps only the ACK it draws.
    pub(in crate::mux) fn on_inbound(&self, raw: &[u8]) {
        if !is_response(raw) || !resp_status(raw).is_some_and(|s| s >= 200) {
            return;
        }
        if cseq_method_token(raw).as_deref() != Some("INVITE") {
            return;
        }
        let (Some(cid), Some(cseq)) = (call_id(raw), cseq_number(raw)) else { return };
        let mut holds = self.0.lock().unwrap();
        let Some(held) = holds.get_mut(&cid).filter(|h| h.cseq == cseq) else { return };
        if resp_status(raw).is_some_and(|s| s < 300) {
            holds.remove(&cid);
        } else if matches!(held.state, HeldState::Open(_)) {
            let _ = held.on_final(raw);
        }
    }

    /// The hold of dialog `call_id`, removed.
    pub(in crate::mux) fn take(&self, call_id: &str) -> Option<Held> {
        self.0.lock().unwrap().remove(call_id)
    }
}
