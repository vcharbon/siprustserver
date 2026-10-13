//! [`Refusals`] — the answers the proxy gives requests it refuses itself.
//!
//! Every final the proxy answers itself carries a To-tag derived from the
//! request ([`StatelessTagger`]), so every copy of one request draws the same
//! tag (RFC 3261 §8.2.7). A refused new INVITE draws a 503 with a jittered,
//! floored `Retry-After` and no `Reason`, and is remembered for 64·T1 (Timer
//! B, the longest a UAC retransmits an INVITE): every copy of it within that
//! window is answered with the same bytes and never judged again, so one
//! INVITE transaction is never both refused and admitted. A copy arriving
//! after the window, or after the memo forgot it past its capacity, is judged
//! afresh.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use load_shed::retry_after;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::{HeaderValue, RetryAfter};
use sip_message::types::SipHeader;
use sip_message::{SipRequest, SipResponse};
use sip_txn::timers::TIMER_B;
use sip_txn::{IdGen, StatelessTagger};

use crate::cancel_lru::retransmit_key;

/// The most refused INVITEs a memo holds by default; past it the oldest is
/// forgotten first. At 64·T1 it covers refusals up to 2 048 a second.
pub const REFUSED_MEMO_MAX: usize = sip_txn::REFUSED_MEMO_MAX;

/// The proxy's refusals: one To-tag secret and one memo of refused new
/// INVITEs, shared by every recv shard so a copy arriving on another socket
/// draws the same answer.
///
/// Concurrency: one mutex guards the memo; [`repeat`](Self::repeat) skips it
/// while nothing is remembered, read from a lock-free mirror.
pub struct Refusals {
    tagger: StatelessTagger,
    jitter_sec: u32,
    hasher: RandomState,
    memo: Mutex<Memo>,
    remembering: AtomicBool,
}

/// The refused INVITEs (keyed hashes of the received transaction, as the
/// retransmission memo keys it), each with the `Retry-After` its 503 carried,
/// in refusal order for expiry.
struct Memo {
    refused: HashMap<u64, (u64, u32)>,
    order: VecDeque<(u64, u64)>,
    max: usize,
}

impl Memo {
    fn expire(&mut self, now_ms: u64) {
        while let Some(&(at_ms, key)) = self.order.front() {
            if now_ms.saturating_sub(at_ms) < TIMER_B && self.order.len() <= self.max {
                break;
            }
            self.order.pop_front();
            if self.refused.get(&key).is_some_and(|&(at, _)| at == at_ms) {
                self.refused.remove(&key);
            }
        }
    }
}

impl Refusals {
    /// Refusals whose `Retry-After` is spread over `jitter_sec` past its base,
    /// remembering at most [`REFUSED_MEMO_MAX`] refused INVITEs. The To-tag
    /// secret is drawn from `id_gen` once, here.
    pub fn new(id_gen: &IdGen, jitter_sec: u32) -> Self {
        Self::with_capacity(id_gen, jitter_sec, REFUSED_MEMO_MAX)
    }

    /// [`new`](Self::new), remembering at most `max` refused INVITEs.
    pub fn with_capacity(id_gen: &IdGen, jitter_sec: u32, max: usize) -> Self {
        Self {
            tagger: StatelessTagger::from_id_gen(id_gen),
            jitter_sec,
            hasher: RandomState::new(),
            memo: Mutex::new(Memo { refused: HashMap::new(), order: VecDeque::new(), max }),
            remembering: AtomicBool::new(false),
        }
    }

    /// The To-tag of any final the proxy answers `req` with itself.
    pub fn to_tag(&self, req: &SipRequest) -> String {
        self.tagger.for_request(req).0
    }

    /// Refuse the new INVITE `req` at `now_ms`, `retry_after_sec` being the
    /// refusing stage's hint: its 503, remembered so every copy within 64·T1
    /// draws the same one.
    pub fn refuse(&self, req: &SipRequest, retry_after_sec: u32, now_ms: u64) -> SipResponse {
        let (_, roll) = self.tagger.for_request(req);
        let retry_after_sec = retry_after::jittered(retry_after_sec, self.jitter_sec, || roll);
        let key = self.key(req);
        let mut memo = self.lock();
        memo.refused.insert(key, (now_ms, retry_after_sec));
        memo.order.push_back((now_ms, key));
        memo.expire(now_ms);
        self.mirror(&memo);
        drop(memo);
        self.answer(req, retry_after_sec)
    }

    /// The 503 an earlier copy of `req` drew, when `req` is a copy of an
    /// INVITE refused within 64·T1 of `now_ms`.
    pub fn repeat(&self, req: &SipRequest, now_ms: u64) -> Option<SipResponse> {
        if !self.remembering.load(Ordering::Relaxed) {
            return None;
        }
        let key = self.key(req);
        let mut memo = self.lock();
        memo.expire(now_ms);
        self.mirror(&memo);
        let &(_, retry_after_sec) = memo.refused.get(&key)?;
        drop(memo);
        Some(self.answer(req, retry_after_sec))
    }

    fn answer(&self, req: &SipRequest, retry_after_sec: u32) -> SipResponse {
        let retry_after = RetryAfter::new(retry_after_sec.to_string());
        let opts = GenerateResponseOpts {
            to_tag: Some(self.to_tag(req)),
            extra_headers: vec![SipHeader {
                name: retry_after.name().as_wire_str().into(),
                value: retry_after.to_wire().into(),
            }],
            ..Default::default()
        };
        generate_response(req, 503, "Service Unavailable", &opts)
    }

    /// The received transaction of `req`, keyed as the retransmission memo
    /// keys it: a genuine copy repeats every field (§17.2.3).
    fn key(&self, req: &SipRequest) -> u64 {
        let from = req.from();
        self.hasher.hash_one(retransmit_key(
            req.top_via(),
            req.call_id().as_str(),
            from.tag(),
            req.method().as_str(),
            req.cseq().seq(),
        ))
    }

    fn lock(&self) -> MutexGuard<'_, Memo> {
        self.memo.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn mirror(&self, memo: &Memo) {
        self.remembering.store(!memo.refused.is_empty(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
