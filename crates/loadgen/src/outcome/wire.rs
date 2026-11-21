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

use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use sip_message::header::{cause_key, retry_after_seconds};
use sip_message::sip_str::SipStr;
use sip_message::sniff::{header_values, session_description, Identity};

use crate::mux::{Datagram, Dir, LossTap, Side, WireTap};

mod excuse;
mod finals;
mod received;

pub use excuse::Refusal;
pub(crate) use finals::Finals;
pub(crate) use finals::Pending;
use finals::{pending_key, settle_pending, unanswered, Request};
use received::note_request;
pub use received::{LegRequest, LEG_REQUESTS_MAX, LEG_REQUEST_BODY_MAX};

/// One failed in-dialog request the caller sent: its non-2xx final, or no
/// final at all (`code: None`).
#[derive(Debug, Clone, PartialEq)]
pub struct WireFailure {
    pub at_ms: i64,
    pub method: String,
    pub code: Option<u16>,
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
    /// The in-dialog requests the leg received, a retransmission once, the
    /// first [`LEG_REQUESTS_MAX`].
    pub received: Vec<LegRequest>,
    /// The leg's own in-dialog requests answered with a non-2xx final.
    pub failures: Vec<WireFailure>,
    /// The leg's own in-dialog requests awaiting a final, by CSeq number:
    /// their method and when they were first sent.
    pub(crate) pending: Pending,
    pub(crate) finals: Finals,
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
    pub(crate) pending: Pending,
    pub(crate) finals: Finals,
    /// The verdict resolved: the facts no longer change.
    pub(crate) sealed: bool,
    /// How many of the caller's failures the seal found: those after it are
    /// the finals of requests it left pending.
    pub(crate) failures_at_seal: usize,
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
        let awaits = |pending: &Pending| pending.values().any(|(method, _)| method != "BYE");
        awaits(&self.pending) || self.legs.iter().any(|l| awaits(&l.pending))
    }
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
        let mut f = self.facts.lock().unwrap();
        if !f.sealed {
            f.sealed = true;
            f.failures_at_seal = f.failures.len();
        }
    }

    /// Whether an in-dialog request other than a BYE still awaits its final
    /// ([`WireFacts::awaits_finals`]).
    pub fn awaits(&self) -> bool {
        self.facts.lock().unwrap().awaits_finals()
    }

    /// The loss tap that tells the facts which of a side's requests the loss
    /// model dropped: on their way out, or on their way to it (an in-dialog
    /// request the side was still sent, a BYE whose copy it missed).
    pub fn loss_tap(&self) -> LossTap {
        let (clock, facts) = (self.clock.clone(), self.facts.clone());
        Arc::new(move |side, dir, d, _| {
            let id = d.id();
            let (None, Some(method), Some(cseq)) = (id.status, id.method, id.cseq) else {
                return;
            };
            let mut f = facts.lock().unwrap();
            let finals = match side {
                Side::Caller => Some(&mut f.finals),
                Side::Leg(role) => {
                    f.legs.iter_mut().find(|l| l.role == role).map(|l| &mut l.finals)
                }
            };
            let Some(finals) = finals else { return };
            match dir {
                Dir::Out => finals.sent.lost(cseq),
                Dir::In if in_dialog(method, id) => {
                    let now = clock.now_ms();
                    finals.took(method, cseq, now);
                    if method == "BYE" {
                        finals.bye_lost(cseq, now);
                    }
                }
                Dir::In => {}
            }
        })
    }

    /// Forget the in-dialog failures the `refusals` excuse ([`Refusal`]): a
    /// UA of the call refused a request the loss model's drops explain, and
    /// the system under test relayed that refusal to another side. The
    /// dialog stays read as gone while a 481 failure from before the seal
    /// stays.
    pub fn excuse(&self, refusals: &[Refusal]) {
        let mut f = self.facts.lock().unwrap();
        let kept = excuse::kept(&f.failures, None, refusals);
        let before_seal = if f.sealed { f.failures_at_seal } else { f.failures.len() };
        let gone = f.failures.iter().zip(&kept).take(before_seal);
        let dialog_gone = gone.into_iter().any(|(w, k)| *k && w.code == Some(481));
        f.saw_481 &= dialog_gone;
        excuse::retain(&mut f.failures, &kept);
        for leg in &mut f.legs {
            let kept = excuse::kept(&leg.failures, Some(&leg.role), refusals);
            excuse::retain(&mut leg.failures, &kept);
        }
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

/// Whether the request `method` (`id`) is an in-dialog request that takes a
/// final of its own: tagged, neither an ACK nor a CANCEL.
fn in_dialog(method: &str, id: &Identity<'_>) -> bool {
    !matches!(method, "ACK" | "CANCEL") && !untagged(id)
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
                let side = (&mut self.pending, &mut self.failures, &mut self.finals);
                if let Some(key) = pending_key(id) {
                    settle_pending(side, key, (code, d.raw()), now);
                }
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
                    self.in_dialog_final(id, (code, d.raw()), now);
                }
            }
            (Dir::In, None) => {
                if in_dialog(method, id) {
                    self.finals.took(method, cseq, now);
                }
                if id.method == Some("BYE") {
                    self.bye_received.get_or_insert(None);
                    if self.finals.bye(&mut self.failures, false, cseq, now) {
                        self.bye_elsewhere(None, now);
                    }
                    self.saw_481 &= self.failures.iter().any(|f| f.code == Some(481));
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
                    if self.finals.bye(&mut self.failures, true, cseq, now) {
                        self.bye_elsewhere(None, now);
                    }
                }
                self.finals.sent(&mut self.failures, method, cseq);
                if let Some(key) = pending_key(id) {
                    self.pending.entry(key).or_insert_with(|| (method.to_string(), now));
                }
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

    fn in_dialog_final(&mut self, id: &Identity<'_>, (code, raw): (u16, &[u8]), now: i64) {
        let Some(key) = pending_key(id) else { return };
        let cseq = key.1;
        let Some((method, sent_at)) = self.pending.remove(&key) else { return };
        if method == "BYE" {
            self.bye = Some(Some(code));
        }
        let req = Request { method: &method, cseq, sent_at };
        if self.finals.judge(&mut self.failures, &req, (code, raw), now) {
            self.saw_481 |= code == 481;
        }
    }

    fn on_leg(&mut self, role: &str, dir: Dir, d: &Datagram<'_>, now: i64) {
        let id = d.id();
        let opened = self.legs.iter().position(|l| l.role == role);
        if self.sealed {
            // Only the finals of the leg's requests the seal left pending.
            if let (Dir::In, Some(code), Some(i), Some(key)) =
                (dir, id.status, opened, pending_key(id))
            {
                let leg = &mut self.legs[i];
                let side = (&mut leg.pending, &mut leg.failures, &mut leg.finals);
                settle_pending(side, key, (code, d.raw()), now);
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
            (Dir::In, None, Some(i)) => {
                let leg = &mut self.legs[i];
                if let (Some(method), Some(cseq)) = (id.method, id.cseq) {
                    if in_dialog(method, id) {
                        leg.finals.took(method, cseq, now);
                    }
                }
                let new_bye = match (id.method, id.cseq) {
                    (Some("BYE"), Some(cseq)) => {
                        leg.finals.bye(&mut leg.failures, false, cseq, now)
                    }
                    _ => false,
                };
                note_request(&mut leg.received, d, now);
                if new_bye {
                    self.bye_elsewhere(Some(i), now);
                }
            }
            (Dir::Out, None, Some(i)) => {
                if self.legs[i].sent_request(id, now) {
                    self.bye_elsewhere(Some(i), now);
                }
            }
            (Dir::In, Some(code), Some(i)) => self.legs[i].own_final(id, (code, d.raw()), now),
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

    /// A BYE first left or reached side `side` (`None`: the caller) at `now`:
    /// every other side's finals learn it, with the requests that side was
    /// sent ([`Finals::other_bye`]).
    fn bye_elsewhere(&mut self, side: Option<usize>, now: i64) {
        let took = match side {
            None => self.finals.requests_taken(),
            Some(i) => self.legs[i].finals.requests_taken(),
        };
        if side.is_some() {
            self.finals.other_bye(&mut self.failures, took.clone(), now);
        }
        for (i, leg) in self.legs.iter_mut().enumerate() {
            if side != Some(i) {
                leg.finals.other_bye(&mut leg.failures, took.clone(), now);
            }
        }
    }
}

impl LegFacts {
    /// An in-dialog request the leg sent (ACK and CANCEL aside), first
    /// sending. Returns whether it is a BYE the leg had not sent before.
    fn sent_request(&mut self, id: &Identity<'_>, now: i64) -> bool {
        let (Some(method), Some(cseq)) = (id.method, id.cseq) else { return false };
        if method == "ACK" || method == "CANCEL" || untagged(id) {
            return false;
        }
        let new_bye = method == "BYE" && self.finals.bye(&mut self.failures, true, cseq, now);
        self.finals.sent(&mut self.failures, method, cseq);
        if let Some(key) = pending_key(id) {
            self.pending.entry(key).or_insert_with(|| (method.to_string(), now));
        }
        new_bye
    }

    /// A final to one of the leg's own in-dialog requests.
    fn own_final(&mut self, id: &Identity<'_>, (code, raw): (u16, &[u8]), now: i64) {
        let Some(key) = pending_key(id).filter(|_| code >= 200) else { return };
        let cseq = key.1;
        let Some((method, sent_at)) = self.pending.remove(&key) else { return };
        let req = Request { method: &method, cseq, sent_at };
        self.finals.judge(&mut self.failures, &req, (code, raw), now);
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
