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
//! A copy arriving after the window, or after the memo forgot it past its
//! capacity, is judged afresh. A CANCEL is never remembered: the CANCEL of a
//! remembered INVITE refusal is answered 200 here, the proxy being the UAS of
//! that INVITE (§9.2).

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
use sip_txn::timers::TIMER_B;
use sip_txn::{IdGen, StatelessTagger};

use crate::cancel_lru::retransmit_key;

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

/// One refusal: when it was made, the `Retry-After` its 503 carried and the
/// To-tag it was answered with.
struct Refused {
    at_ms: u64,
    retry_after_sec: u32,
    to_tag: Box<str>,
}

/// One stripe's refused requests (keyed hashes of the received transaction,
/// as the retransmission memo keys it), in refusal order for expiry.
struct Memo {
    refused: HashMap<u64, Refused>,
    order: VecDeque<(u64, u64)>,
    max: usize,
}

impl Memo {
    /// Forget the refusals past 64·T1 and the oldest past the stripe's
    /// capacity; the number forgotten.
    fn expire(&mut self, now_ms: u64) -> usize {
        let before = self.refused.len();
        while let Some(&(at_ms, key)) = self.order.front() {
            if now_ms.saturating_sub(at_ms) < TIMER_B && self.order.len() <= self.max {
                break;
            }
            self.order.pop_front();
            if self.refused.get(&key).is_some_and(|r| r.at_ms == at_ms) {
                self.refused.remove(&key);
            }
        }
        before - self.refused.len()
    }
}

impl Refusals {
    /// Refusals whose `Retry-After` is spread over `jitter_sec` past its base,
    /// remembering at most [`REFUSED_MEMO_MAX`] refused requests. The To-tag
    /// secret is drawn from `id_gen` once, here.
    pub fn new(id_gen: &IdGen, jitter_sec: u32) -> Self {
        Self::with_capacity(id_gen, jitter_sec, REFUSED_MEMO_MAX)
    }

    /// [`new`](Self::new), remembering at most about `max` refused requests:
    /// each stripe holds `max / STRIPES`, rounded up.
    pub fn with_capacity(id_gen: &IdGen, jitter_sec: u32, max: usize) -> Self {
        let per_stripe = max.div_ceil(STRIPES).max(1);
        Self {
            tagger: StatelessTagger::from_id_gen(id_gen),
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
        if req.method() != Method::Cancel {
            let key = self.key(req, req.method().as_str());
            let refused =
                Refused { at_ms: now_ms, retry_after_sec, to_tag: to_tag.as_str().into() };
            let mut memo = self.lock(key);
            let added = usize::from(memo.refused.insert(key, refused).is_none());
            memo.order.push_back((now_ms, key));
            let forgotten = memo.expire(now_ms);
            self.count(added, forgotten);
        }
        answer(req, src, 503, "Service Unavailable", to_tag, Some(retry_after_sec))
    }

    /// The 503 an earlier copy of `req` drew, answering `req` received from
    /// `src`, when `req` is a copy of a request refused within 64·T1 of
    /// `now_ms` whose lines a response can echo.
    pub fn repeat(&self, req: &SipRequest, src: SocketAddr, now_ms: u64) -> Option<SipResponse> {
        let refused = self.remembered(req, req.method().as_str(), now_ms)?;
        answer(req, src, 503, "Service Unavailable", refused.0, Some(refused.1)).ok()
    }

    /// The 200 answering `cancel`, received from `src`, when it cancels an
    /// INVITE refused within 64·T1 of `now_ms` (§9.2): the INVITE already has
    /// its final, and the 200 carries that final's To-tag.
    pub fn cancel(&self, cancel: &SipRequest, src: SocketAddr, now_ms: u64) -> Option<SipResponse> {
        let (to_tag, _) = self.remembered(cancel, Method::Invite.as_str(), now_ms)?;
        answer(cancel, src, 200, "OK", to_tag, None).ok()
    }

    /// The To-tag and `Retry-After` of the remembered `method` refusal of
    /// `req`'s transaction.
    fn remembered(&self, req: &SipRequest, method: &str, now_ms: u64) -> Option<(String, u32)> {
        if self.remembered.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let key = self.key(req, method);
        let mut memo = self.lock(key);
        let forgotten = memo.expire(now_ms);
        self.count(0, forgotten);
        let refused = memo.refused.get(&key)?;
        Some((refused.to_tag.to_string(), refused.retry_after_sec))
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
        incoming_source: Some((src.ip().to_string(), src.port())),
        ..Default::default()
    };
    generate_response(req, status, reason, &opts)
}

#[cfg(test)]
mod tests;
