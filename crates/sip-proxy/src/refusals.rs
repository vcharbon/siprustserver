//! [`Refusals`] — the answers the proxy gives requests it refuses itself.
//!
//! Every final the proxy answers itself carries a To-tag derived from the
//! request ([`StatelessTagger`]), so every copy of one request draws the same
//! tag (RFC 3261 §8.2.7). A request the proxy refuses (an INVITE at the
//! self-gate, any request a failed worker selection leaves without a target)
//! draws a 503 with a jittered, floored `Retry-After` and no `Reason`, and is
//! remembered for 64·T1 (Timer B, the longest a UAC retransmits a request):
//! every copy of it within that window is answered with the same 503 and
//! never judged again, so one transaction is never both refused and admitted.
//! Every answer records the source of the request it answers on its topmost
//! Via (RFC 3261 §18.2.1, RFC 3581 §4).
//! A request whose forward the socket refused as past the largest UDP payload
//! draws a 513 Message Too Large (§21.5.14), remembered the same way, so a copy
//! is answered without another forward.
//! A request a check before forwarding rejects (483, 482, 420, 400, 416, or
//! a stickiness token it cannot decode) is remembered the same way in a memo
//! of its own, its copies answered by that check again; so is the 400 to an
//! in-dialog INVITE the parser refused. Such a reject answers no CANCEL here,
//! which is routed on; it claims the ACK of the INVITE it rejected.
//! Every copy is answered on the RFC schedule (ADR-0063 §1): the copies up to
//! the re-sends of its sender's ladder at any pace, a later one only T1 after
//! the last answered one, none later than 64·T1 after the first; the others
//! are absorbed, and each copy keeps the refusal 64·T1 longer.
//! A copy arriving after the memo forgot it, 64·T1 after its last copy or
//! past its capacity, is judged afresh. A CANCEL is never remembered: the
//! CANCEL of a remembered INVITE refusal is answered 200 here, on its own
//! schedule, the proxy being the UAS of that INVITE (§9.2).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::SocketAddr;

use load_shed::retry_after;
use sip_message::generators::{generate_response, GenerateError, GenerateResponseOpts};
use sip_message::header::{HeaderValue, RetryAfter, Via};
use sip_message::types::SipHeader;
use sip_message::{Method, RequestPartial, SipRequest, SipResponse};
use sip_txn::{IdGen, StatelessTagger};

use crate::cancel_lru::retransmit_key;
use memo::{Copies, Refused, Striped};
use sip_message::received::observed_source;

/// The most refused requests a memo holds by default; past it the oldest is
/// forgotten first. At 64·T1 it covers refusals up to 2 048 a second.
pub const REFUSED_MEMO_MAX: usize = sip_txn::REFUSED_MEMO_MAX;

/// The proxy's refusals: one To-tag secret and two striped memos, shared by
/// every recv shard so a copy arriving on another socket draws the same
/// answer: one of the requests refused 503 or 513, one of the preflight
/// rejects. Each memo has its own capacity, so a flood of one kind never
/// pushes the other out.
pub struct Refusals {
    tagger: StatelessTagger,
    jitter_sec: u32,
    hasher: RandomState,
    refused: Striped,
    preflight: Striped,
}

/// The final a remembered refusal draws.
#[derive(Clone, Copy)]
enum Answer {
    /// A 503 with the `Retry-After` it carried.
    Unavailable { retry_after_sec: u32 },
    /// A 513: the request cannot be forwarded in one datagram.
    TooLarge,
}

impl Answer {
    fn to(
        self,
        req: &SipRequest,
        src: SocketAddr,
        to_tag: String,
    ) -> Result<SipResponse, GenerateError> {
        match self {
            Answer::Unavailable { retry_after_sec } => {
                answer(req, src, 503, "Service Unavailable", to_tag, Some(retry_after_sec))
            }
            Answer::TooLarge => answer(req, src, 513, "Message Too Large", to_tag, None),
        }
    }
}

/// What a copy of a request the proxy answered itself draws (ADR-0063 §1).
#[derive(Debug)]
pub enum Repeat {
    /// The answer again: the copy is on its sender's schedule.
    Answer(Box<SipResponse>),
    /// Nothing: the copy is past it.
    Absorbed,
}

impl Refusals {
    /// Refusals whose `Retry-After` is spread over `jitter_sec` past its base,
    /// remembering at most [`REFUSED_MEMO_MAX`] refused requests. The To-tag
    /// secret is drawn from `id_gen` once, here: this instance's own, for a
    /// proxy with no shared key (tests, development).
    pub fn new(id_gen: &IdGen, jitter_sec: u32) -> Self {
        Self::with_capacity(id_gen, jitter_sec, REFUSED_MEMO_MAX)
    }

    /// [`new`](Self::new), remembering at most about `max` refused requests
    /// and as many preflight rejects: each stripe holds `max / STRIPES`,
    /// rounded up.
    pub fn with_capacity(id_gen: &IdGen, jitter_sec: u32, max: usize) -> Self {
        Self::keyed(StatelessTagger::from_id_gen(id_gen), jitter_sec, max)
    }

    /// Refusals whose To-tags `tagger` derives (a deployment's shared key,
    /// `crate::security::own_final_tag`), remembering at most about `max`
    /// refused requests and as many preflight rejects.
    pub fn keyed(tagger: StatelessTagger, jitter_sec: u32, max: usize) -> Self {
        Self {
            tagger,
            jitter_sec,
            hasher: RandomState::new(),
            refused: Striped::new(max),
            preflight: Striped::new(max),
        }
    }

    /// The To-tag of any final the proxy answers `req` with itself.
    pub fn to_tag(&self, req: &SipRequest) -> String {
        self.tagger.for_request(req).0
    }

    /// Whether `ack` acknowledges a non-2xx final the proxy answered an
    /// INVITE with itself: its To-tag is the one this tagger derives for that
    /// INVITE (RFC 3261 §17.1.1.3).
    pub fn acknowledges_own_final(&self, ack: &SipRequest) -> bool {
        self.tagger.acknowledges_own_final(ack)
    }

    /// Refuse `req`, received from `src`, at `now_ms`, `retry_after_sec`
    /// being the refusing stage's hint: its 503, remembered so every copy
    /// within 64·T1 draws the same one. A CANCEL is answered but not
    /// remembered.
    pub fn refuse(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        retry_after_sec: u32,
        now_ms: u64,
    ) -> Result<SipResponse, GenerateError> {
        let (to_tag, roll) = self.tagger.for_request(req);
        let retry_after_sec = retry_after::jittered(retry_after_sec, self.jitter_sec, || roll);
        self.remember(req, src, now_ms, Answer::Unavailable { retry_after_sec }, to_tag)
    }

    /// Refuse `req`, received from `src`, at `now_ms`, whose forward the socket
    /// refused as past the largest UDP payload: its 513, remembered as
    /// [`refuse`](Self::refuse) remembers a 503.
    pub fn refuse_too_large(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        now_ms: u64,
    ) -> Result<SipResponse, GenerateError> {
        let to_tag = self.to_tag(req);
        self.remember(req, src, now_ms, Answer::TooLarge, to_tag)
    }

    /// Remember `req`'s refusal at `now_ms` as drawing `answer` with `to_tag`,
    /// unless it is a CANCEL, and build that answer to `src`.
    fn remember(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        now_ms: u64,
        answer: Answer,
        to_tag: String,
    ) -> Result<SipResponse, GenerateError> {
        if req.method() != Method::Cancel {
            let key = self.key(req, req.method().as_str());
            let refused =
                Refused::first(req.method(), now_ms, Some(answer), to_tag.as_str().into());
            self.refused.note(key, refused, now_ms);
        }
        answer.to(req, src, to_tag)
    }

    /// Whether the reject a check this hop runs before forwarding composes
    /// for `req` (483, 482, 420, 400, 416, or a stickiness token it cannot
    /// decode), received at `now_ms`, is owed: at its first arrival,
    /// remembered then in the preflight memo, and for a copy on its sender's
    /// schedule ([`Repeat`]).
    pub fn preflight(&self, req: &SipRequest, now_ms: u64) -> bool {
        let key = self.key(req, req.method().as_str());
        match self.preflight.with(key, now_ms, |rejected| rejected.copies.copy(now_ms)) {
            Some(owed) => owed,
            None => {
                let rejected = Refused::first(req.method(), now_ms, None, self.to_tag(req).into());
                self.preflight.note(key, rejected, now_ms);
                true
            }
        }
    }

    /// Remember, at `now_ms`, the 400 this hop answered the in-dialog INVITE
    /// whose parse the parser refused with `partial`: the 400 echoes the
    /// dialog's To-tag, so only the memo recognises its ACK
    /// ([`acknowledges_refusal`](Self::acknowledges_refusal)). A copy keeps
    /// it 64·T1 longer. Any other request is not remembered.
    pub fn remember_parse_refused(&self, partial: &RequestPartial, now_ms: u64) {
        let Some(dialog_tag) = partial.to().tag() else { return };
        if *partial.method() != Method::Invite {
            return;
        }
        let (via, call_id) = (partial.via().first(), partial.call_id().as_str());
        let key = self.key_of(via, call_id, partial.from().tag(), "INVITE", partial.cseq().seq());
        let copy = self.preflight.with(key, now_ms, |rejected| rejected.copies.copy(now_ms));
        if copy.is_none() {
            let rejected = Refused::first(&Method::Invite, now_ms, None, dialog_tag.into());
            self.preflight.note(key, rejected, now_ms);
        }
    }

    /// What `req`, received from `src`, draws when it is a copy of a request
    /// refused 503 or 513 within 64·T1 of its last copy at `now_ms` whose
    /// lines a response can echo: that final again on its sender's schedule.
    pub fn repeat(&self, req: &SipRequest, src: SocketAddr, now_ms: u64) -> Option<Repeat> {
        let (owed, to_tag, answer) = self
            .with_refusal(req, req.method().as_str(), now_ms, |refused| {
                let answer = refused.answer?;
                Some((refused.copies.copy(now_ms), refused.to_tag.to_string(), answer))
            })
            .flatten()?;
        let resp = answer.to(req, src, to_tag).ok()?;
        Some(if owed { Repeat::Answer(Box::new(resp)) } else { Repeat::Absorbed })
    }

    /// What `cancel`, received from `src`, draws when it cancels an INVITE
    /// refused 503 or 513 within 64·T1 of its last copy at `now_ms` (§9.2):
    /// the INVITE already has its final, so a 200 under that final's To-tag,
    /// on the CANCEL's own schedule.
    pub fn cancel(&self, cancel: &SipRequest, src: SocketAddr, now_ms: u64) -> Option<Repeat> {
        let (owed, to_tag) = self
            .with_refusal(cancel, Method::Invite.as_str(), now_ms, |refused| {
                refused.answer?;
                let owed = match refused.cancel.as_mut() {
                    Some(copies) => copies.copy(now_ms),
                    None => {
                        refused.cancel = Some(Copies::first(&Method::Cancel, now_ms));
                        true
                    }
                };
                Some((owed, refused.to_tag.to_string()))
            })
            .flatten()?;
        let resp = answer(cancel, src, 200, "OK", to_tag, None).ok()?;
        Some(if owed { Repeat::Answer(Box::new(resp)) } else { Repeat::Absorbed })
    }

    /// Whether `ack` acknowledges a final this hop answered its INVITE with
    /// itself, remembered within 64·T1 of the INVITE's last copy at `now_ms`:
    /// a 503 or 513 refusal, or the reject of a check before forwarding
    /// ([`preflight`](Self::preflight)). The To-tag a final to an in-dialog
    /// INVITE echoes is its dialog's, so only the memo tells; the ACK carries
    /// the INVITE's branch (§17.1.1.3).
    pub fn acknowledges_refusal(&self, ack: &SipRequest, now_ms: u64) -> bool {
        let refused =
            self.with_refusal(ack, Method::Invite.as_str(), now_ms, |r| r.answer.is_some());
        let key = self.key(ack, Method::Invite.as_str());
        refused.unwrap_or(false) || self.preflight.with(key, now_ms, |_| ()).is_some()
    }

    /// `read` of the remembered 503 or 513 refusal of `req`'s transaction as
    /// a `method` request, not lapsed at `now_ms`.
    fn with_refusal<T>(
        &self,
        req: &SipRequest,
        method: &str,
        now_ms: u64,
        read: impl FnOnce(&mut Refused) -> T,
    ) -> Option<T> {
        self.refused.with(self.key(req, method), now_ms, read)
    }

    /// The received transaction of `req` as a `method` request, keyed as the
    /// retransmission memo keys it: a genuine copy repeats every field
    /// (§17.2.3), and a CANCEL every field of its INVITE but the method.
    fn key(&self, req: &SipRequest, method: &str) -> u64 {
        let from = req.from();
        self.key_of(req.top_via(), req.call_id().as_str(), from.tag(), method, req.cseq().seq())
    }

    fn key_of(
        &self,
        via: &Via,
        call_id: &str,
        from_tag: Option<&str>,
        method: &str,
        cseq: u32,
    ) -> u64 {
        self.hasher.hash_one(retransmit_key(via, call_id, from_tag, method, cseq))
    }
}

/// The proxy's own `status` final to `req`, received from `src`, with
/// `to_tag` and, for a refusal, its `Retry-After`.
fn answer(
    req: &SipRequest,
    src: SocketAddr,
    status: u16,
    reason: &str,
    to_tag: String,
    retry_after_sec: Option<u32>,
) -> Result<SipResponse, GenerateError> {
    let extra_headers = retry_after_sec
        .map(|sec| {
            let retry_after = RetryAfter::new(sec.to_string());
            SipHeader {
                name: retry_after.name().as_wire_str().into(),
                value: retry_after.to_wire().into(),
            }
        })
        .into_iter()
        .collect();
    let opts = GenerateResponseOpts {
        to_tag: Some(to_tag),
        extra_headers,
        incoming_source: Some(observed_source(src)),
        ..Default::default()
    };
    generate_response(req, status, reason, &opts)
}

mod memo;
#[cfg(test)]
mod tests;
