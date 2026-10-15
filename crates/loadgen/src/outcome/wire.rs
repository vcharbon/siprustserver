//! What one call's endpoints saw on the wire, as the facts its outcome record
//! states: fed by the call's [`WireTap`], every datagram its caller and its
//! callee legs sent or had delivered.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use sip_message::header::{cause_key, readable_reasons};
use sip_message::sip_str::SipStr;
use sip_message::sniff::{
    call_id, content_type_is, cseq_method_token, cseq_number, has_body, header_values, is_response,
    req_method, resp_status, to_tag,
};

use crate::mux::{Dir, Side, WireTap};

/// One failure the wire showed: an in-dialog request the caller sent that drew
/// a non-2xx final.
#[derive(Debug, Clone, PartialEq)]
pub struct WireFailure {
    pub at_ms: i64,
    pub method: String,
    pub code: u16,
}

/// What one callee leg did with the INVITE that opened it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegFacts {
    pub role: String,
    /// The final the leg sent to its INVITE.
    pub final_code: Option<u16>,
    /// An 18x with a session description went out before the final.
    pub early_media: bool,
    /// A CANCEL reached the leg.
    pub cancelled: bool,
}

/// The caller's facts, and each callee leg's, in the order the legs opened.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WireFacts {
    /// The Call-ID of the caller's first INVITE.
    pub call_id: Option<String>,
    pub(crate) invite_cseq: Option<u32>,
    pub started_at_ms: Option<i64>,
    pub first_provisional_at_ms: Option<i64>,
    pub setup_ended_at_ms: Option<i64>,
    pub answered_at_ms: Option<i64>,
    pub final_code: Option<u16>,
    /// The final's cause key (`<protocol>;cause=<n>`), when its Reason header
    /// states one.
    pub final_cause: Option<String>,
    pub early_media: bool,
    /// The caller sent a CANCEL.
    pub cancelled: bool,
    /// The caller's BYE and the final it drew (`Some(None)`: sent, no final).
    pub bye: Option<Option<u16>>,
    /// The caller received a BYE, and the final it answered it with.
    pub bye_received: Option<Option<u16>>,
    /// An in-dialog request or the BYE drew a 481: the dialog was gone.
    pub saw_481: bool,
    pub failures: Vec<WireFailure>,
    pub legs: Vec<LegFacts>,
    /// The caller's in-dialog requests awaiting a final, by CSeq number.
    pub(crate) pending: HashMap<u32, String>,
}

/// A call's wire observer: hand [`tap`](Self::tap) to its mux network, read
/// [`facts`](Self::facts) when the call is done.
pub struct CallWire {
    clock: Clock,
    facts: Arc<Mutex<WireFacts>>,
}

impl CallWire {
    pub fn new(clock: Clock) -> Self {
        Self { clock, facts: Arc::default() }
    }

    /// The tap the call's mux network feeds.
    pub fn tap(&self) -> WireTap {
        let (clock, facts) = (self.clock.clone(), self.facts.clone());
        Arc::new(move |side, dir, raw| {
            let now = clock.now_ms();
            let mut f = facts.lock().unwrap();
            match side {
                Side::Caller => f.on_caller(dir, raw, now),
                Side::Leg(role) => f.on_leg(role, dir, raw),
            }
        })
    }

    /// The facts seen so far.
    pub fn facts(&self) -> WireFacts {
        self.facts.lock().unwrap().clone()
    }
}

/// Whether `raw` carries a session description.
fn has_sdp(raw: &[u8]) -> bool {
    has_body(raw) && content_type_is(raw, "application/sdp")
}

impl WireFacts {
    fn on_caller(&mut self, dir: Dir, raw: &[u8], now: i64) {
        let (Some(method), Some(cseq)) = (cseq_method_token(raw), cseq_number(raw)) else {
            return;
        };
        match (dir, is_response(raw)) {
            (Dir::Out, false) => {
                if self.started_at_ms.is_none() && method == "INVITE" {
                    self.call_id = call_id(raw);
                }
                self.caller_sent(&method, cseq, now);
            }
            (Dir::In, true) => {
                let Some(code) = resp_status(raw) else { return };
                let setup = method == "INVITE"
                    && self.invite_cseq == Some(cseq)
                    && call_id(raw) == self.call_id;
                if setup {
                    self.setup_response(code, raw, now);
                } else if code >= 200 {
                    self.in_dialog_final(cseq, code, now);
                }
            }
            (Dir::In, false) if req_method(raw).as_deref() == Some("BYE") => {
                self.bye_received.get_or_insert(None);
            }
            (Dir::Out, true) if method == "BYE" => {
                let code = resp_status(raw);
                if code.is_some_and(|c| c >= 200) {
                    self.bye_received = Some(code);
                }
            }
            _ => {}
        }
    }

    fn caller_sent(&mut self, method: &str, cseq: u32, now: i64) {
        match method {
            "INVITE" if self.started_at_ms.is_none() => {
                self.started_at_ms = Some(now);
                self.invite_cseq = Some(cseq);
            }
            "INVITE" if self.invite_cseq == Some(cseq) => {}
            "ACK" => {}
            "CANCEL" => self.cancelled = true,
            _ => {
                if method == "BYE" {
                    self.bye.get_or_insert(None);
                }
                self.pending.entry(cseq).or_insert_with(|| method.to_string());
            }
        }
    }

    fn setup_response(&mut self, code: u16, raw: &[u8], now: i64) {
        if self.setup_ended_at_ms.is_some() {
            return;
        }
        if code < 200 {
            self.first_provisional_at_ms.get_or_insert(now);
            self.early_media |= code > 100 && has_sdp(raw);
            return;
        }
        self.setup_ended_at_ms = Some(now);
        self.final_code = Some(code);
        if code < 300 {
            self.answered_at_ms = Some(now);
        } else {
            let lines = header_values(raw, "Reason").into_iter().map(SipStr::from);
            self.final_cause = cause_key(&readable_reasons(lines));
        }
    }

    fn in_dialog_final(&mut self, cseq: u32, code: u16, now: i64) {
        let Some(method) = self.pending.remove(&cseq) else { return };
        if method == "BYE" {
            self.bye = Some(Some(code));
        }
        self.saw_481 |= code == 481;
        if code >= 300 {
            self.failures.push(WireFailure { at_ms: now, method, code });
        }
    }

    fn on_leg(&mut self, role: &str, dir: Dir, raw: &[u8]) {
        let opened = self.legs.iter().position(|l| l.role == role);
        match (dir, is_response(raw), opened) {
            (Dir::In, false, None) => {
                let initial =
                    req_method(raw).as_deref() == Some("INVITE") && to_tag(raw).is_empty();
                if initial {
                    self.legs.push(LegFacts { role: role.to_string(), ..LegFacts::default() });
                }
            }
            (Dir::In, false, Some(i)) if req_method(raw).as_deref() == Some("CANCEL") => {
                self.legs[i].cancelled = true;
            }
            (Dir::Out, true, Some(i)) if cseq_method_token(raw).as_deref() == Some("INVITE") => {
                let leg = &mut self.legs[i];
                let Some(code) = resp_status(raw) else { return };
                if leg.final_code.is_some() {
                    return;
                }
                if code >= 200 {
                    leg.final_code = Some(code);
                } else {
                    leg.early_media |= code > 100 && has_sdp(raw);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
