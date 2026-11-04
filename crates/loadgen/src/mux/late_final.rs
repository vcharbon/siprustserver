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

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use sip_message::generators::{
    generate_ack_for_2xx_from_invite, generate_ack_for_non_2xx, generate_bye_for_2xx_from_invite,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipMessage, SipParser, SipRequest, SipResponse};

use super::datagram::{key_of, Datagram};

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
    /// No final yet: the INVITE itself, as sent.
    Open(Arc<[u8]>),
    /// A non-2xx final arrived: the ACK it drew.
    Completed(Arc<[u8]>),
    /// A 2xx arrived after the call released: its ACK, and the CSeq of the
    /// BYE sent to end the dialog.
    Answered { ack: Arc<[u8]>, bye_cseq: u32 },
}

/// What a final arriving for a held INVITE draws.
pub(in crate::mux) enum Reply {
    /// Nothing for this hold (not its transaction): the datagram is a stray.
    NotHeld,
    /// Absorbed: the final of the BYE the hold sent.
    Absorbed,
    /// The ACK of a non-2xx final.
    AckNon2xx(Arc<[u8]>, SocketAddr),
    /// The ACK of a 2xx, and, the first time, the BYE ending its dialog.
    Answered(Vec<(Arc<[u8]>, SocketAddr)>),
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

fn wire(request: SipRequest) -> Arc<[u8]> {
    serialize(&SipMessage::Request(request)).into()
}

/// A fresh RFC 3261 branch for a request the hold originates.
fn branch() -> String {
    format!("z9hG4bK{}", uuid::Uuid::new_v4().simple())
}

impl Held {
    /// What the final `response` draws from this hold.
    pub(in crate::mux) fn on_final(&mut self, response: &Datagram<'_>) -> Reply {
        let id = response.id();
        let (Some(status), Some(cseq), Some(method)) =
            (response.final_status(), id.cseq, id.cseq_method)
        else {
            return Reply::NotHeld;
        };
        let (dst, held_cseq) = (self.dst, self.cseq);
        match (&self.state, method) {
            (HeldState::Answered { bye_cseq, .. }, "BYE") if *bye_cseq == cseq => Reply::Absorbed,
            _ if method != "INVITE" || cseq != held_cseq => Reply::NotHeld,
            (HeldState::Completed(ack), _) if status >= 300 => Reply::AckNon2xx(ack.clone(), dst),
            (HeldState::Answered { ack, .. }, _) if status < 300 => {
                Reply::Answered(vec![(ack.clone(), dst)])
            }
            (HeldState::Open(invite), _) => {
                let (Some(invite), Some(resp)) =
                    (parse_request(invite), parse_response(response.raw()))
                else {
                    return Reply::NotHeld;
                };
                // A final whose lines no ACK can echo is not the held call's.
                if status >= 300 {
                    let Ok(ack) = generate_ack_for_non_2xx(&invite, &resp, &[]) else {
                        return Reply::NotHeld;
                    };
                    let ack = wire(ack);
                    self.state = HeldState::Completed(ack.clone());
                    return Reply::AckNon2xx(ack, dst);
                }
                let bye_cseq = held_cseq + 1;
                let (Ok(ack), Ok(bye)) = (
                    generate_ack_for_2xx_from_invite(&invite, &resp, &branch(), None),
                    generate_bye_for_2xx_from_invite(&invite, &resp, bye_cseq, &branch()),
                ) else {
                    return Reply::NotHeld;
                };
                let ack = wire(ack);
                self.state = HeldState::Answered { ack: ack.clone(), bye_cseq };
                Reply::Answered(vec![(ack, dst), (wire(bye), dst)])
            }
            _ => Reply::NotHeld,
        }
    }
}

/// A caller endpoint's holds, one per dialog (Call-ID, hashed).
#[derive(Default)]
pub(in crate::mux) struct SentInvites(Mutex<Vec<(u64, Held)>>);

impl SentInvites {
    /// Hold `d` when it is an INVITE request (its retransmission replaces it
    /// with the same bytes).
    pub(in crate::mux) fn on_outbound(&self, d: &Datagram<'_>, dst: SocketAddr) {
        let id = d.id();
        if id.method != Some("INVITE") {
            return;
        }
        let (Some(cid), Some(cseq)) = (id.call_id, id.cseq) else { return };
        let (dialog, held) = (key_of(cid), Held { cseq, dst, state: HeldState::Open(d.shared()) });
        let mut holds = self.0.lock().unwrap();
        match holds.iter_mut().find(|(k, _)| *k == dialog) {
            Some((_, h)) => *h = held,
            None => {
                // A caller endpoint holds a handful of dialogs: room for one more.
                holds.reserve_exact(1);
                holds.push((dialog, held));
            }
        }
    }

    /// A final to the live call's held INVITE: a 2xx ends the hold, a non-2xx
    /// keeps only the ACK it draws.
    pub(in crate::mux) fn on_inbound(&self, d: &Datagram<'_>) {
        let id = d.id();
        let Some(status) = d.final_status() else { return };
        if id.cseq_method != Some("INVITE") {
            return;
        }
        let (Some(cid), Some(cseq)) = (id.call_id, id.cseq) else { return };
        let dialog = key_of(cid);
        let mut holds = self.0.lock().unwrap();
        let Some(i) = holds.iter().position(|(k, h)| *k == dialog && h.cseq == cseq) else {
            return;
        };
        if status < 300 {
            holds.swap_remove(i);
        } else if matches!(holds[i].1.state, HeldState::Open(_)) {
            let _ = holds[i].1.on_final(d);
        }
    }

    /// The hold of dialog `call_id`, removed.
    pub(in crate::mux) fn take(&self, call_id: &str) -> Option<Held> {
        let dialog = key_of(call_id);
        let mut holds = self.0.lock().unwrap();
        let i = holds.iter().position(|(k, _)| *k == dialog)?;
        Some(holds.swap_remove(i).1)
    }
}
