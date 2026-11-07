//! What one call's endpoints saw on the wire, as the facts its outcome record
//! states: fed by the call's [`WireTap`], every datagram its caller and its
//! callee legs sent or had delivered.
//!
//! The facts are sealed when the call's verdict resolves ([`CallWire::seal`]):
//! what the generator's own teardown of a failed call draws afterwards (a
//! CANCEL and its 487, a late final, a cleanup BYE) is not the call's
//! outcome. Of the teardown, the facts keep only whether a BYE released the
//! dialog. An in-dialog request other than a BYE still awaiting its final at
//! the seal is judged by its client transaction (RFC 3261 §17.1.2): a final
//! that arrives after the seal still settles it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use sip_message::header::{cause_key, retry_after_seconds, HeaderValue, SubscriptionState};
use sip_message::sip_str::SipStr;
use sip_message::sniff::{body, header_value, header_values, session_description, Identity};

use crate::mux::{Datagram, Dir, Side, WireTap};

/// One failed in-dialog request the caller sent: its non-2xx final, or no
/// final at all (`code: None`).
#[derive(Debug, Clone, PartialEq)]
pub struct WireFailure {
    pub at_ms: i64,
    pub method: String,
    pub code: Option<u16>,
}

/// One in-dialog request the caller or a callee leg received (ACK and CANCEL
/// aside): what
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

/// The bound on the requests the caller's or a leg's facts keep, and on each
/// one's body.
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
    /// The first 100 Trying to the INVITE.
    pub trying_at_ms: Option<i64>,
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
    /// The in-dialog requests the caller received, a retransmission once, the
    /// first [`LEG_REQUESTS_MAX`].
    pub received: Vec<LegRequest>,
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

    /// Whether an in-dialog request other than a BYE, the caller's or a
    /// callee leg's, still awaits its final.
    pub fn awaits_finals(&self) -> bool {
        let awaits = |pending: &HashMap<u32, (String, i64)>| {
            pending.values().any(|(method, _)| method != "BYE")
        };
        awaits(&self.pending) || self.legs.iter().any(|l| awaits(&l.pending))
    }
}

/// After the seal, the final `code` to the request `method` `cseq`: a
/// request other than a BYE that `pending` still holds is settled, a non-2xx
/// one a failure. A late 481 is that request's failure only: the call's
/// ending stays as the seal left it, never `cut`.
fn settle_pending(
    pending: &mut HashMap<u32, (String, i64)>,
    failures: &mut Vec<WireFailure>,
    method: &str,
    cseq: u32,
    code: u16,
    now: i64,
) {
    let held = pending.get(&cseq).is_some_and(|(m, _)| m == method && m != "BYE");
    if code < 200 || !held {
        return;
    }
    pending.remove(&cseq);
    if code >= 300 {
        failures.push(WireFailure { at_ms: now, method: method.to_string(), code: Some(code) });
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
        Arc::new(move |side, dir, d| {
            let now = clock.now_ms();
            let mut f = facts.lock().unwrap();
            match side {
                Side::Caller => f.on_caller(dir, d, now),
                Side::Leg(role) => f.on_leg(role, dir, d, now),
            }
        })
    }

    /// The call's verdict resolved: freeze the setup facts.
    pub fn seal(&self) {
        self.facts.lock().unwrap().sealed = true;
    }

    /// Whether an in-dialog request other than a BYE still awaits its final
    /// ([`WireFacts::awaits_finals`]).
    pub fn awaits(&self) -> bool {
        self.facts.lock().unwrap().awaits_finals()
    }

    /// The facts seen so far.
    pub fn facts(&self) -> WireFacts {
        self.facts.lock().unwrap().clone()
    }
}

/// Whether `raw` carries a session description.
fn has_sdp(raw: &[u8]) -> bool {
    session_description(raw).is_some()
}

/// Whether the request `id` is out of any dialog: its To has no tag.
fn untagged(id: &Identity<'_>) -> bool {
    id.to_tag.is_none_or(str::is_empty)
}

impl WireFacts {
    fn on_caller(&mut self, dir: Dir, d: &Datagram<'_>, now: i64) {
        let id = d.id();
        let (Some(method), Some(cseq)) = (id.cseq_method, id.cseq) else {
            return;
        };
        if self.sealed {
            // Whether the teardown released the dialog, and the finals of the
            // requests the seal left pending.
            let bye_ok = method == "BYE" && id.status.is_some_and(|c| (200..300).contains(&c));
            self.teardown_released |= bye_ok;
            if let (Dir::In, Some(code)) = (dir, id.status) {
                settle_pending(&mut self.pending, &mut self.failures, method, cseq, code, now);
            }
            return;
        }
        match (dir, id.status) {
            (Dir::Out, None) => self.caller_sent(method, cseq, id, now),
            (Dir::In, Some(code)) => {
                let setup = method == "INVITE"
                    && self.invite_cseq == Some(cseq)
                    && id.call_id == self.call_id.as_deref();
                if setup {
                    self.setup_response(code, d.raw(), now);
                } else if code >= 200 {
                    self.in_dialog_final(cseq, code, now);
                }
            }
            (Dir::In, None) => {
                if id.method == Some("BYE") {
                    self.bye_received.get_or_insert(None);
                }
                note_request(&mut self.received, d, now);
            }
            (Dir::Out, Some(code)) if method == "BYE" && code >= 200 => {
                self.bye_received = Some(Some(code));
            }
            _ => {}
        }
    }

    fn caller_sent(&mut self, method: &str, cseq: u32, id: &Identity<'_>, now: i64) {
        let initial = method == "INVITE" && untagged(id);
        match method {
            "INVITE" if self.started_at_ms.is_none() => {
                self.call_id = id.call_id.map(str::to_string);
                self.started_at_ms = Some(now);
                self.invite_cseq = Some(cseq);
            }
            "INVITE" if self.invite_cseq == Some(cseq) => {}
            // A new initial INVITE on the call's Call-ID (a 401/407/422 retry)
            // is the setup again.
            "INVITE" if initial && id.call_id == self.call_id.as_deref() => {
                self.restart_setup(cseq)
            }
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
        self.trying_at_ms = None;
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
            } else {
                self.trying_at_ms.get_or_insert(now);
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

    fn on_leg(&mut self, role: &str, dir: Dir, d: &Datagram<'_>, now: i64) {
        let id = d.id();
        let opened = self.legs.iter().position(|l| l.role == role);
        if self.sealed {
            // Only the finals of the leg's requests the seal left pending.
            if let (Dir::In, Some(code), Some(i), Some(method), Some(cseq)) =
                (dir, id.status, opened, id.cseq_method, id.cseq)
            {
                let leg = &mut self.legs[i];
                settle_pending(&mut leg.pending, &mut leg.failures, method, cseq, code, now);
            }
            return;
        }
        match (dir, id.status, opened) {
            (Dir::In, None, None) => {
                if id.method == Some("INVITE") && untagged(id) {
                    self.legs.push(LegFacts { role: role.to_string(), ..LegFacts::default() });
                }
            }
            (Dir::In, None, Some(i)) if id.method == Some("CANCEL") => {
                self.legs[i].cancelled = true;
            }
            (Dir::In, None, Some(i)) => note_request(&mut self.legs[i].received, d, now),
            (Dir::Out, None, Some(i)) => self.legs[i].sent_request(id, now),
            (Dir::In, Some(code), Some(i)) => self.legs[i].own_final(id, code, now),
            (Dir::Out, Some(code), Some(i)) if id.cseq_method == Some("INVITE") => {
                let leg = &mut self.legs[i];
                if leg.final_code.is_some() {
                    return;
                }
                if code >= 200 {
                    leg.final_code = Some(code);
                } else {
                    leg.early_media |= code > 100 && has_sdp(d.raw());
                }
            }
            _ => {}
        }
    }
}

impl LegFacts {
    /// An in-dialog request the leg sent (ACK and CANCEL aside), first sending.
    fn sent_request(&mut self, id: &Identity<'_>, now: i64) {
        let (Some(method), Some(cseq)) = (id.method, id.cseq) else { return };
        if method != "ACK" && method != "CANCEL" && !untagged(id) {
            self.pending.entry(cseq).or_insert_with(|| (method.to_string(), now));
        }
    }

    /// A final to one of the leg's own in-dialog requests.
    fn own_final(&mut self, id: &Identity<'_>, code: u16, now: i64) {
        let Some(cseq) = id.cseq else { return };
        if code < 200 {
            return;
        }
        let Some((method, _)) = self.pending.remove(&cseq) else { return };
        if code >= 300 {
            self.failures.push(WireFailure { at_ms: now, method, code: Some(code) });
        }
    }
}

/// Note the in-dialog request `d` received at `now` in `received` (ACK and
/// out-of-dialog requests aside, a retransmission once, the first
/// [`LEG_REQUESTS_MAX`]).
fn note_request(received: &mut Vec<LegRequest>, d: &Datagram<'_>, now: i64) {
    let id = d.id();
    let (Some(method), Some(cseq)) = (id.method, id.cseq) else { return };
    let seen = received.iter().any(|r| r.cseq == cseq && r.method == method);
    if method == "ACK" || untagged(id) || seen || received.len() >= LEG_REQUESTS_MAX {
        return;
    }
    let raw = d.raw();
    let subscription_state = header_value(raw, "Subscription-State")
        .and_then(|v| SubscriptionState::parse(&SipStr::owned(&v)).ok())
        .map(|s| s.token().to_ascii_lowercase());
    let body = body(raw).unwrap_or_default();
    received.push(LegRequest {
        at_ms: now,
        method: method.to_string(),
        cseq,
        subscription_state,
        body: body[..body.len().min(LEG_REQUEST_BODY_MAX)].to_vec(),
    });
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
