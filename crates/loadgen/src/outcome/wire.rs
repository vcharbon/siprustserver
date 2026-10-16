//! What one call's endpoints saw on the wire, as the facts its outcome record
//! states: fed by the call's [`WireTap`], every datagram its caller and its
//! callee legs sent or had delivered.
//!
//! The facts are sealed when the call's verdict resolves ([`CallWire::seal`]):
//! what the generator's own teardown of a failed call draws afterwards (a
//! CANCEL and its 487, a late final, a cleanup BYE) is not the call's
//! outcome. Of the teardown, the facts keep only whether a BYE released the
//! dialog.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use sip_message::header::{cause_key, retry_after_seconds, HeaderValue, SubscriptionState};
use sip_message::sip_str::SipStr;
use sip_message::sniff::{
    body, call_id, content_type_is, cseq_method_token, cseq_number, has_body, header_value,
    header_values, is_response, req_method, resp_status, to_tag,
};

use crate::mux::{Dir, Side, WireTap};

/// One failed in-dialog request the caller sent: its non-2xx final, or no
/// final at all (`code: None`).
#[derive(Debug, Clone, PartialEq)]
pub struct WireFailure {
    pub at_ms: i64,
    pub method: String,
    pub code: Option<u16>,
}

/// One in-dialog request a callee leg received (ACK and CANCEL aside): what
/// it said, for a reader that tells one outcome of an in-dialog exchange from
/// another (a NOTIFY's subscription state and sipfrag, an INFO's payload).
#[derive(Debug, Clone, PartialEq)]
pub struct LegRequest {
    pub at_ms: i64,
    pub method: String,
    pub(crate) cseq: u32,
    /// The `Subscription-State` value's state token (RFC 6665 §8.2.3).
    pub subscription_state: Option<String>,
    /// The body's first [`LEG_REQUEST_BODY_MAX`] bytes.
    pub body: Vec<u8>,
}

/// The bound on the requests a leg's facts keep, and on each one's body.
pub const LEG_REQUESTS_MAX: usize = 8;
pub const LEG_REQUEST_BODY_MAX: usize = 64;

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
    /// The in-dialog requests the leg received, a retransmission once, the
    /// first [`LEG_REQUESTS_MAX`].
    pub received: Vec<LegRequest>,
    /// The leg's own in-dialog requests answered with a non-2xx final.
    pub failures: Vec<WireFailure>,
    /// The leg's own in-dialog requests awaiting a final, by CSeq number:
    /// their method and when they were first sent.
    pub(crate) pending: HashMap<u32, (String, i64)>,
}

impl LegFacts {
    /// The leg's own in-dialog requests other than a BYE that never drew a
    /// final, as failures without a code, in the order they were sent.
    pub fn unanswered(&self) -> Vec<WireFailure> {
        unanswered(&self.pending)
    }
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
    /// The final's cause key (`<protocol>;cause=<n>`), when the first value
    /// of its Reason header states one.
    pub final_cause: Option<String>,
    /// The delta-seconds of the final's Retry-After, when it carried one.
    pub final_retry_after_s: Option<u32>,
    pub early_media: bool,
    /// The caller sent a CANCEL before the verdict resolved.
    pub cancelled: bool,
    /// The caller's BYE and the final it drew (`Some(None)`: sent, no final).
    pub bye: Option<Option<u16>>,
    /// The caller received a BYE, and the final it answered it with.
    pub bye_received: Option<Option<u16>>,
    /// An in-dialog request or the BYE drew a 481: the dialog was gone.
    pub saw_481: bool,
    /// In-dialog requests answered with a non-2xx final.
    pub failures: Vec<WireFailure>,
    pub legs: Vec<LegFacts>,
    /// The caller's in-dialog requests awaiting a final, by CSeq number: their
    /// method and when they were first sent.
    pub(crate) pending: HashMap<u32, (String, i64)>,
    /// The verdict resolved: the facts no longer change.
    pub(crate) sealed: bool,
    /// After the verdict, the generator's teardown BYE drew a 2xx.
    pub teardown_released: bool,
}

impl WireFacts {
    /// The caller's in-dialog requests other than the BYE that never drew a
    /// final, as failures without a code, in the order they were sent.
    pub fn unanswered(&self) -> Vec<WireFailure> {
        unanswered(&self.pending)
    }
}

/// The requests of `pending` other than a BYE, as failures without a code,
/// in the order they were sent.
fn unanswered(pending: &HashMap<u32, (String, i64)>) -> Vec<WireFailure> {
    let mut left: Vec<WireFailure> = pending
        .values()
        .filter(|(method, _)| method != "BYE")
        .map(|(method, at_ms)| WireFailure { at_ms: *at_ms, method: method.clone(), code: None })
        .collect();
    left.sort_by_key(|f| f.at_ms);
    left
}

/// A call's wire observer: hand [`tap`](Self::tap) to its mux network,
/// [`seal`](Self::seal) it when the verdict resolves, read
/// [`facts`](Self::facts) when the call is done.
#[derive(Clone)]
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
                Side::Leg(role) => f.on_leg(role, dir, raw, now),
            }
        })
    }

    /// The call's verdict resolved: freeze the setup facts.
    pub fn seal(&self) {
        self.facts.lock().unwrap().sealed = true;
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
        if self.sealed {
            // Only whether the teardown released the dialog.
            let bye_ok =
                method == "BYE" && resp_status(raw).is_some_and(|c| (200..300).contains(&c));
            self.teardown_released |= bye_ok;
            return;
        }
        match (dir, is_response(raw)) {
            (Dir::Out, false) => self.caller_sent(&method, cseq, raw, now),
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

    fn caller_sent(&mut self, method: &str, cseq: u32, raw: &[u8], now: i64) {
        let initial = method == "INVITE" && to_tag(raw).is_empty();
        match method {
            "INVITE" if self.started_at_ms.is_none() => {
                self.call_id = call_id(raw);
                self.started_at_ms = Some(now);
                self.invite_cseq = Some(cseq);
            }
            "INVITE" if self.invite_cseq == Some(cseq) => {}
            // A new initial INVITE on the call's Call-ID (a 401/407/422 retry)
            // is the setup again.
            "INVITE" if initial && call_id(raw) == self.call_id => self.restart_setup(cseq),
            "ACK" => {}
            "CANCEL" => self.cancelled = true,
            _ => {
                if method == "BYE" {
                    self.bye.get_or_insert(None);
                }
                self.pending.entry(cseq).or_insert_with(|| (method.to_string(), now));
            }
        }
    }

    /// Forget the previous attempt's setup facts; the call's start stays.
    fn restart_setup(&mut self, cseq: u32) {
        self.invite_cseq = Some(cseq);
        self.first_provisional_at_ms = None;
        self.setup_ended_at_ms = None;
        self.answered_at_ms = None;
        self.final_code = None;
        self.final_cause = None;
        self.final_retry_after_s = None;
        self.early_media = false;
    }

    fn setup_response(&mut self, code: u16, raw: &[u8], now: i64) {
        if self.setup_ended_at_ms.is_some() {
            return;
        }
        if code < 200 {
            // 100 Trying is the next hop's own acknowledgement (RFC 3261
            // §8.2.6.1): the setup's progress starts at the first other one.
            if code > 100 {
                self.first_provisional_at_ms.get_or_insert(now);
                self.early_media |= has_sdp(raw);
            }
            return;
        }
        self.setup_ended_at_ms = Some(now);
        self.final_code = Some(code);
        if code < 300 {
            self.answered_at_ms = Some(now);
        } else {
            self.final_cause =
                cause_key(header_values(raw, "Reason").into_iter().map(SipStr::from));
            self.final_retry_after_s = retry_after_seconds(
                header_values(raw, "Retry-After").into_iter().map(SipStr::from),
            );
        }
    }

    fn in_dialog_final(&mut self, cseq: u32, code: u16, now: i64) {
        let Some((method, _)) = self.pending.remove(&cseq) else { return };
        if method == "BYE" {
            self.bye = Some(Some(code));
        }
        self.saw_481 |= code == 481;
        if code >= 300 {
            self.failures.push(WireFailure { at_ms: now, method, code: Some(code) });
        }
    }

    fn on_leg(&mut self, role: &str, dir: Dir, raw: &[u8], now: i64) {
        if self.sealed {
            return;
        }
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
            (Dir::In, false, Some(i)) => self.legs[i].received_request(raw, now),
            (Dir::Out, false, Some(i)) => self.legs[i].sent_request(raw, now),
            (Dir::In, true, Some(i)) => self.legs[i].own_final(raw, now),
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

impl LegFacts {
    /// An in-dialog request the leg sent (ACK and CANCEL aside), first sending.
    fn sent_request(&mut self, raw: &[u8], now: i64) {
        let (Some(method), Some(cseq)) = (req_method(raw), cseq_number(raw)) else { return };
        if method != "ACK" && method != "CANCEL" && !to_tag(raw).is_empty() {
            self.pending.entry(cseq).or_insert((method, now));
        }
    }

    /// A final to one of the leg's own in-dialog requests.
    fn own_final(&mut self, raw: &[u8], now: i64) {
        let (Some(code), Some(cseq)) = (resp_status(raw), cseq_number(raw)) else { return };
        if code < 200 {
            return;
        }
        let Some((method, _)) = self.pending.remove(&cseq) else { return };
        if code >= 300 {
            self.failures.push(WireFailure { at_ms: now, method, code: Some(code) });
        }
    }

    fn received_request(&mut self, raw: &[u8], now: i64) {
        let (Some(method), Some(cseq)) = (req_method(raw), cseq_number(raw)) else { return };
        let in_dialog = !to_tag(raw).is_empty();
        let seen = self.received.iter().any(|r| r.cseq == cseq && r.method == method);
        if method == "ACK" || !in_dialog || seen || self.received.len() >= LEG_REQUESTS_MAX {
            return;
        }
        let subscription_state = header_value(raw, "Subscription-State")
            .and_then(|v| SubscriptionState::parse(&SipStr::owned(&v)).ok())
            .map(|s| s.token().to_ascii_lowercase());
        let body = body(raw).unwrap_or_default();
        self.received.push(LegRequest {
            at_ms: now,
            method,
            cseq,
            subscription_state,
            body: body[..body.len().min(LEG_REQUEST_BODY_MAX)].to_vec(),
        });
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
