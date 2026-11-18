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
//! A request a check before forwarding rejects (483, 482, 420, 400, 416) is
//! remembered the same way, its copies answered by that check again; such a
//! reject answers no CANCEL here and claims no ACK: both are routed on.
//! Every copy is answered on the RFC schedule (ADR-0063 §1): the copies up to
//! the re-sends of its sender's ladder at any pace, a later one only T1 after
//! the last answered one, none later than 64·T1 after the first; the others
//! are absorbed, and each copy keeps the refusal 64·T1 longer.
//! A copy arriving after the memo forgot it, 64·T1 after its last copy or
//! past its capacity, is judged afresh. A CANCEL is never remembered: the
//! CANCEL of a remembered INVITE refusal is answered 200 here, on its own
//! schedule, the proxy being the UAS of that INVITE (§9.2).

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use load_shed::retry_after;
use sip_message::generators::{generate_response, GenerateError, GenerateResponseOpts};
use sip_message::header::{HeaderValue, RetryAfter};
use sip_message::types::SipHeader;
use sip_message::{Method, SipRequest, SipResponse};
use sip_retransmit::Class;
use sip_txn::timers::TIMER_B;
use sip_txn::{IdGen, StatelessTagger};
use std::time::Duration;

use crate::cancel_lru::retransmit_key;
use sip_message::received::observed_source;

/// The most refused requests a memo holds by default; past it the oldest is
/// forgotten first. At 64·T1 it covers refusals up to 2 048 a second.
pub const REFUSED_MEMO_MAX: usize = sip_txn::REFUSED_MEMO_MAX;

/// The memo's independently locked stripes. A copy hashes to the stripe its
/// original went to, so striping changes no answer; it bounds the requests
/// that contend on one lock to those whose keys share a stripe.
const STRIPES: usize = 16;

/// The proxy's refusals: one To-tag secret and one striped memo of refused
/// requests, shared by every recv shard so a copy arriving on another socket
/// draws the same answer.
///
/// Concurrency: each stripe's mutex guards that stripe's refusals; a lookup
/// takes one stripe's lock, and none while nothing is remembered anywhere
/// (read from a lock-free count).
pub struct Refusals {
    tagger: StatelessTagger,
    jitter_sec: u32,
    hasher: RandomState,
    stripes: [Mutex<Memo>; STRIPES],
    remembered: AtomicUsize,
}

/// One refusal: the copies of its request, those of a CANCEL of it, the
/// final it drew (`None` for a preflight reject, which its check composes
/// again) and the To-tag it was answered with.
struct Refused {
    copies: Copies,
    cancel: Option<Copies>,
    answer: Option<Answer>,
    to_tag: Box<str>,
}

/// The copies of one request the proxy answered itself (ADR-0063 §1): when
/// it first arrived, how many copies followed, when the last one arrived and
/// when the last answered one did, in ms.
#[derive(Clone, Copy)]
struct Copies {
    class: Class,
    first_ms: u64,
    copies: u32,
    last_ms: u64,
    last_answered_ms: u64,
}

impl Copies {
    /// The first arrival at `now_ms` of a request of `method`, which its
    /// sender re-sends on Timer A (an INVITE) or Timer E.
    fn first(method: &Method, now_ms: u64) -> Self {
        let class = match method {
            Method::Invite => Class::InviteClient,
            Method::Cancel => Class::CancelClient,
            _ => Class::NonInviteClient,
        };
        Self { class, first_ms: now_ms, copies: 0, last_ms: now_ms, last_answered_ms: now_ms }
    }

    /// Count a copy arriving at `now_ms`; whether it is answered: inside the
    /// 64·T1 of the first arrival and owed on the sender's ladder.
    fn copy(&mut self, now_ms: u64) -> bool {
        self.copies = self.copies.saturating_add(1);
        self.last_ms = now_ms;
        let since_answered = Duration::from_millis(now_ms.saturating_sub(self.last_answered_ms));
        let owed = now_ms.saturating_sub(self.first_ms) < TIMER_B
            && self.class.owes_copy(self.copies, Some(since_answered));
        if owed {
            self.last_answered_ms = now_ms;
        }
        owed
    }

    /// Whether 64·T1 passed at `now_ms` since the last copy.
    fn lapsed(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_ms) >= TIMER_B
    }
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
    /// The final `req`, received from `src`, draws, with `to_tag`.
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

/// One place in [`Memo::order`]: the ms it is due to be looked at, the key,
/// and the first arrival of the refusal it was queued for.
type Queued = (u64, u64, u64);

/// One stripe's refused requests (keyed hashes of the received transaction,
/// as the retransmission memo keys it), queued in the order they are due to
/// be looked at for expiry.
struct Memo {
    refused: HashMap<u64, Refused>,
    order: VecDeque<Queued>,
    max: usize,
}

impl Memo {
    /// Forget the refusals 64·T1 past their last copy and the oldest past
    /// the stripe's capacity; the number forgotten. A queued refusal that
    /// took a copy since is queued again at that copy; a place whose
    /// refusal is gone or was made again is stale and dropped.
    fn expire(&mut self, now_ms: u64) -> usize {
        let before = self.refused.len();
        while let Some(&(at_ms, key, first_ms)) = self.order.front() {
            let over = self.order.len() > self.max;
            if !over && now_ms.saturating_sub(at_ms) < TIMER_B {
                break;
            }
            self.order.pop_front();
            let Some(refused) = self.refused.get(&key) else { continue };
            if refused.copies.first_ms != first_ms {
                continue;
            }
            if over || refused.copies.lapsed(now_ms) {
                self.refused.remove(&key);
            } else {
                self.order.push_back((refused.copies.last_ms, key, first_ms));
            }
        }
        before - self.refused.len()
    }
}

impl Refusals {
    /// Refusals whose `Retry-After` is spread over `jitter_sec` past its base,
    /// remembering at most [`REFUSED_MEMO_MAX`] refused requests. The To-tag
    /// secret is drawn from `id_gen` once, here: this instance's own, for a
    /// proxy with no shared key (tests, development).
    pub fn new(id_gen: &IdGen, jitter_sec: u32) -> Self {
        Self::with_capacity(id_gen, jitter_sec, REFUSED_MEMO_MAX)
    }

    /// [`new`](Self::new), remembering at most about `max` refused requests:
    /// each stripe holds `max / STRIPES`, rounded up.
    pub fn with_capacity(id_gen: &IdGen, jitter_sec: u32, max: usize) -> Self {
        Self::keyed(StatelessTagger::from_id_gen(id_gen), jitter_sec, max)
    }

    /// Refusals whose To-tags `tagger` derives (a deployment's shared key,
    /// `crate::security::own_final_tag`), remembering at most about `max`
    /// refused requests.
    pub fn keyed(tagger: StatelessTagger, jitter_sec: u32, max: usize) -> Self {
        let per_stripe = max.div_ceil(STRIPES).max(1);
        Self {
            tagger,
            jitter_sec,
            hasher: RandomState::new(),
            stripes: std::array::from_fn(|_| {
                Mutex::new(Memo {
                    refused: HashMap::new(),
                    order: VecDeque::new(),
                    max: per_stripe,
                })
            }),
            remembered: AtomicUsize::new(0),
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
            self.note(req, now_ms, Some(answer), to_tag.as_str().into());
        }
        answer.to(req, src, to_tag)
    }

    /// Remember `req`'s refusal at `now_ms` as its first arrival.
    fn note(&self, req: &SipRequest, now_ms: u64, answer: Option<Answer>, to_tag: Box<str>) {
        let key = self.key(req, req.method().as_str());
        let copies = Copies::first(req.method(), now_ms);
        let refused = Refused { copies, cancel: None, answer, to_tag };
        let mut memo = self.lock(key);
        let added = usize::from(memo.refused.insert(key, refused).is_none());
        memo.order.push_back((now_ms, key, now_ms));
        let forgotten = memo.expire(now_ms);
        self.count(added, forgotten);
    }

    /// Whether the reject a check this hop runs before forwarding composes
    /// for `req` (483, 482, 420, 400, 416), received at `now_ms`, is owed: at
    /// its first arrival, remembered then, and for a copy on its sender's
    /// schedule ([`Repeat`]).
    pub fn preflight(&self, req: &SipRequest, now_ms: u64) -> bool {
        let first = self.with_refusal(req, req.method().as_str(), now_ms, |refused| {
            refused.copies.copy(now_ms)
        });
        match first {
            Some(owed) => owed,
            None => {
                self.note(req, now_ms, None, self.to_tag(req).into());
                true
            }
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

    /// Whether `ack` acknowledges a 503 or 513 this memo holds for its
    /// INVITE, refused within 64·T1 of its last copy at `now_ms`: the To-tag
    /// a refusal of an in-dialog INVITE echoes is its dialog's, so only the
    /// memo tells (§17.1.1.3).
    pub fn acknowledges_refusal(&self, ack: &SipRequest, now_ms: u64) -> bool {
        self.with_refusal(ack, Method::Invite.as_str(), now_ms, |r| r.answer.is_some())
            .unwrap_or(false)
    }

    /// `read` of the remembered `method` refusal of `req`'s transaction, not
    /// lapsed at `now_ms`.
    fn with_refusal<T>(
        &self,
        req: &SipRequest,
        method: &str,
        now_ms: u64,
        read: impl FnOnce(&mut Refused) -> T,
    ) -> Option<T> {
        if self.remembered.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let key = self.key(req, method);
        let mut memo = self.lock(key);
        let forgotten = memo.expire(now_ms);
        self.count(0, forgotten);
        memo.refused.get_mut(&key).filter(|r| !r.copies.lapsed(now_ms)).map(read)
    }

    /// The received transaction of `req` as a `method` request, keyed as the
    /// retransmission memo keys it: a genuine copy repeats every field
    /// (§17.2.3), and a CANCEL every field of its INVITE but the method.
    fn key(&self, req: &SipRequest, method: &str) -> u64 {
        let from = req.from();
        self.hasher.hash_one(retransmit_key(
            req.top_via(),
            req.call_id().as_str(),
            from.tag(),
            method,
            req.cseq().seq(),
        ))
    }

    fn lock(&self, key: u64) -> MutexGuard<'_, Memo> {
        // STRIPES divides 256: the key's low byte picks the stripe its
        // remainder by STRIPES names.
        let slot = usize::from(key.to_le_bytes()[0]) % STRIPES;
        #[expect(clippy::indexing_slicing, reason = "a remainder of STRIPES indexes the stripes")]
        let stripe = &self.stripes[slot];
        crate::poison::locked(stripe, crate::poison::REFUSALS_MEMO)
    }

    fn count(&self, added: usize, forgotten: usize) {
        if added > 0 {
            self.remembered.fetch_add(added, Ordering::Relaxed);
        }
        if forgotten > 0 {
            self.remembered.fetch_sub(forgotten, Ordering::Relaxed);
        }
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

#[cfg(test)]
mod tests;
