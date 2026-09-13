//! Feeding the per-leg message ring ([`call::MessageRing`]) from a turn: what
//! the transaction layer handed the call and what it sent on the call's
//! behalf for it, then what the turn itself puts on the wire. One entry per
//! distinct message, in handling order — a repeat the layer hands the TU
//! again, or a retained datagram re-sent, adds nothing.
//!
//! The header values an entry keeps are read off the parsed message by
//! `sip-message` here, once; nothing downstream re-reads a datagram.

use call::helpers::{confirmed_dialog, find_leg, record_message};
use call::{Call, MessageDirection, MessageEntry, Obligation};
use sip_message::{HeaderName, Method, SipMessage, SipRequest, SipResponse};

use crate::config::B2buaConfig;
use crate::effects::{OutboundBody, OutboundSipEffect, Provenance};
use crate::event::CallEvent;

/// The ring's configuration for one turn: the cap and the header names
/// resolved once. `None` while the ring is off, so an unconfigured stack pays
/// for nothing.
pub(crate) struct Ring {
    cap: usize,
    names: Vec<HeaderName>,
}

impl Ring {
    pub(crate) fn of(config: &B2buaConfig) -> Option<Self> {
        if config.cdr.message_ring == 0 {
            return None;
        }
        Some(Self {
            cap: config.cdr.message_ring,
            names: config
                .cdr
                .captured_headers
                .iter()
                .map(|n| HeaderName::from(n.as_str()))
                .collect(),
        })
    }

    /// Record what the layer handed the call on `leg_id` — the received
    /// message, or a CANCEL it answered itself — with the messages it sent
    /// autonomously for it: the 100 Trying every INVITE server transaction
    /// answers with, the hop-by-hop ACK of a non-2xx INVITE final on a client
    /// transaction this node holds, the 200 and 487 a matched CANCEL draws.
    /// `discharged` is the obligation the received request retired, the
    /// executor's own verdict on whether an ACK is the first of its 2xx.
    pub(crate) fn received(
        &self,
        call: Call,
        leg_id: &str,
        event: &CallEvent,
        discharged: Option<&Obligation>,
        now_ms: i64,
    ) -> Call {
        match event {
            CallEvent::Sip { message, matched_client_txn, .. } => {
                if repeated(&call, leg_id, message, discharged) {
                    return call;
                }
                match message.as_ref() {
                    SipMessage::Request(req) if req.method() == Method::Invite => {
                        self.invite_received(call, leg_id, req, now_ms)
                    }
                    SipMessage::Request(req) => {
                        self.record(call, leg_id, self.request_entry(req, now_ms))
                    }
                    SipMessage::Response(resp)
                        if *matched_client_txn
                            && resp.status() >= 300
                            && resp.cseq().method() == Method::Invite =>
                    {
                        let call = self.record(call, leg_id, self.response_entry(resp, now_ms));
                        let ack = MessageEntry {
                            direction: MessageDirection::Authored,
                            method: Method::Ack.to_string(),
                            code: None,
                            headers: Vec::new(),
                            ..self.response_entry(resp, now_ms)
                        };
                        self.record(call, leg_id, ack)
                    }
                    SipMessage::Response(resp) => {
                        self.record(call, leg_id, self.response_entry(resp, now_ms))
                    }
                }
            }
            CallEvent::Cancelled { invite_cseq, in_dialog, headers, .. } => {
                let cseq = invite_cseq.unwrap_or(0);
                let own_tag = call::helpers::b2bua_tag(&call, leg_id);
                // The CANCEL names the INVITE's dialog: this stack's tag when
                // the INVITE was in one, none when it was not.
                let cancel_tag = if *in_dialog { own_tag.clone() } else { None };
                let cancel = MessageEntry {
                    seq: 0,
                    at_ms: now_ms,
                    direction: MessageDirection::Received,
                    method: Method::Cancel.to_string(),
                    cseq,
                    code: None,
                    to_tag: cancel_tag.clone(),
                    decision_ordinal: 0,
                    headers: sip_message::capture::captured_headers(headers, &self.names),
                };
                let cancel_ok = MessageEntry {
                    direction: MessageDirection::Authored,
                    code: Some(200),
                    headers: Vec::new(),
                    ..cancel.clone()
                };
                let terminated = MessageEntry {
                    direction: MessageDirection::Authored,
                    method: Method::Invite.to_string(),
                    code: Some(487),
                    to_tag: own_tag,
                    headers: Vec::new(),
                    ..cancel.clone()
                };
                let call = self.record(call, leg_id, cancel);
                let call = self.record(call, leg_id, cancel_ok);
                self.record(call, leg_id, terminated)
            }
            _ => call,
        }
    }

    /// Record an INVITE received on `leg_id` and the 100 Trying the
    /// transaction layer answered it with before the call saw it.
    pub(crate) fn invite_received(
        &self,
        call: Call,
        leg_id: &str,
        req: &SipRequest,
        now_ms: i64,
    ) -> Call {
        let call = self.record(call, leg_id, self.request_entry(req, now_ms));
        let trying = MessageEntry {
            direction: MessageDirection::Authored,
            code: Some(100),
            headers: Vec::new(),
            ..self.request_entry(req, now_ms)
        };
        self.record(call, leg_id, trying)
    }

    /// Record what a turn puts on the wire, in emission order. A retained
    /// datagram's repeat is the message it repeats and adds nothing; an
    /// emission naming no leg belongs to no ring.
    pub(crate) fn sent(&self, call: Call, outbound: &[OutboundSipEffect], now_ms: i64) -> Call {
        outbound.iter().fold(call, |call, eff| {
            let Some(leg_id) = eff.leg_id.as_deref() else { return call };
            let direction = match eff.provenance {
                Provenance::Relayed => MessageDirection::Relayed,
                Provenance::Authored => MessageDirection::Authored,
            };
            let entry = match &eff.body {
                OutboundBody::Request(req) => self.request_entry(req, now_ms),
                OutboundBody::Response(resp) => self.response_entry(resp, now_ms),
                OutboundBody::Datagram(_) => return call,
            };
            self.record(call, leg_id, MessageEntry { direction, ..entry })
        })
    }

    fn record(&self, call: Call, leg_id: &str, entry: MessageEntry) -> Call {
        record_message(call, leg_id, self.cap, entry)
    }

    /// A received request's entry; `seq` is assigned at the append.
    fn request_entry(&self, req: &SipRequest, now_ms: i64) -> MessageEntry {
        MessageEntry {
            seq: 0,
            at_ms: now_ms,
            direction: MessageDirection::Received,
            method: req.method().to_string(),
            cseq: req.cseq().seq(),
            code: None,
            to_tag: req.to().tag().map(str::to_string),
            decision_ordinal: 0,
            headers: req.captured_headers(&self.names),
        }
    }

    /// A received response's entry; `seq` is assigned at the append.
    fn response_entry(&self, resp: &SipResponse, now_ms: i64) -> MessageEntry {
        MessageEntry {
            seq: 0,
            at_ms: now_ms,
            direction: MessageDirection::Received,
            method: resp.cseq().method().to_string(),
            cseq: resp.cseq().seq(),
            code: Some(resp.status()),
            to_tag: resp.to().tag().map(str::to_string),
            decision_ordinal: 0,
            headers: resp.captured_headers(&self.names),
        }
    }
}

/// Whether the layer handed the TU a message it already handled: an ACK that
/// names a dialog this leg holds yet discharges no 2xx awaiting it (RFC 3261
/// §13.3.1.4), a copy of a 2xx the dialog already ACKed (§13.2.2.4), or a
/// reliable provisional already relayed or PRACKed (RFC 3262 §4) — the same
/// readings the executor absorbs and re-ACKs by.
fn repeated(
    call: &Call,
    leg_id: &str,
    message: &SipMessage,
    discharged: Option<&Obligation>,
) -> bool {
    let Some(leg) = find_leg(call, leg_id) else { return false };
    match message {
        SipMessage::Request(req) => {
            req.method() == Method::Ack
                && discharged.is_none()
                && req.to().tag().is_some_and(|tag| {
                    call::helpers::holds_local_tag(call, leg_id, tag) == Some(true)
                })
        }
        SipMessage::Response(resp) => {
            confirmed_dialog(leg)
                .or_else(|| leg.dialogs.first())
                .is_some_and(|d| crate::rules::relay::retransmitted_2xx(d, resp))
                || crate::rules::relay::repeated_reliable_provisional(call, leg_id, resp)
        }
    }
}
