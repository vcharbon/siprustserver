//! The node's refusals of new INVITEs before any transaction holds them: the
//! one answer such an INVITE draws, and the memo of the INVITE identities
//! refused or held by a live server transaction. Every stage that judges a
//! new INVITE shares one instance — an ingress hook ahead of the receive queue
//! and this layer's deferred-backlog ceiling — so every copy of one INVITE
//! draws the same bytes wherever it is judged (RFC 3261 §8.2.7), and a copy
//! of an INVITE a live server transaction holds is left to that transaction
//! (§17.2.1). A copy of a refused INVITE draws its refusal on the RFC
//! schedule (ADR-0063 §1), and the memo keeps the refusal until 64·T1 after
//! its last copy.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use sip_message::generators::GenerateError;
use sip_message::{SipRequest, SipResponse};
use sip_retransmit::{Class, COPY_WINDOW};
use tokio::time::Instant;

use super::copies::Copies;

/// Builds the response that refuses a new INVITE. It answers one request
/// identically each time: every later copy of that INVITE is sent the same
/// response again (RFC 3261 §8.2.7). An INVITE whose lines no response can
/// echo is refused with none.
pub type NewCallRefusal =
    Arc<dyn Fn(&SipRequest) -> Result<SipResponse, GenerateError> + Send + Sync>;

/// The most refused identities a memo holds by default; past its capacity the
/// oldest is forgotten first, and a later copy of that INVITE is judged
/// afresh. At 64·T1 it covers refusals up to 2 048 a second.
pub const REFUSED_MEMO_MAX: usize = 65_536;

/// What [`InviteRefusals::refuse`] decided for an INVITE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A live server transaction holds the INVITE: the copy is that
    /// transaction's to answer.
    Held,
    /// The INVITE is refused; `first` when no copy of it was refused in the
    /// last 64·T1.
    Refused { first: bool },
}

/// What a copy of a refused INVITE draws (ADR-0063 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedCopy {
    /// The refusal again: the copy is on Timer A's schedule.
    Answered,
    /// Nothing: the copy is past the schedule.
    Absorbed,
}

/// What the transaction layer decides for an INVITE no transaction matches.
pub(super) enum Admission {
    /// A copy of a refused INVITE: it draws that refusal again, or nothing
    /// past the schedule.
    Repeat(RefusedCopy),
    /// Refused now, at the caller's ceiling.
    Refuse,
    /// Admitted; `held` when its identity is now held for a transaction.
    Admit { held: bool },
}

/// One node's refusals of new INVITEs. Clone shares the instance.
///
/// Concurrency: one mutex guards the refused and held identities; each call is one
/// critical section, so no identity is both. A refusal skips a held identity, an
/// admission repeats a refused one, a seed's hold clears its refusal (the call is
/// here). [`remembers`](Self::remembers) is a lock-free mirror; a stale `false`
/// only passes a copy on to the layer, which repeats the refusal.
#[derive(Clone)]
pub struct InviteRefusals {
    shared: Arc<Shared>,
}

struct Shared {
    answer: NewCallRefusal,
    hasher: RandomState,
    memo: Mutex<Memo>,
    remembering: AtomicBool,
    /// Copies of refused INVITEs absorbed past the schedule, by every stage.
    absorbed: AtomicU64,
}

/// One place in [`Memo::order`]: the instant it is due to be looked at, the
/// identity, and the first arrival of the refusal it was queued for.
type Queued = (Instant, u64, Instant);

/// The identities (top-`Via` branch, Call-ID, From-tag) as keyed hashes:
/// those refused, each with the copies of its INVITE, until 64·T1 after its
/// last copy (Timer B, the longest a UAC retransmits an INVITE), queued in
/// the order they are due to be looked at; and those a live server
/// transaction holds, with the count of transactions holding each.
struct Memo {
    refused: HashMap<u64, Copies>,
    order: VecDeque<Queued>,
    held: HashMap<u64, u32>,
    max: usize,
}

impl Memo {
    /// Forget the refusals lapsed at `now`, and the oldest past the
    /// capacity. A queued refusal that took a copy since is queued again at
    /// that copy; a place whose refusal is gone or was refused again is
    /// stale and dropped.
    fn expire(&mut self, now: Instant) {
        while let Some(&(at, key, first)) = self.order.front() {
            let over = self.order.len() > self.max;
            if !over && now.saturating_duration_since(at) < COPY_WINDOW {
                break;
            }
            self.order.pop_front();
            let Some(copies) = self.refused.get(&key).filter(|c| c.first_arrival() == first) else {
                continue;
            };
            if over || copies.lapsed(now) {
                self.refused.remove(&key);
            } else {
                self.order.push_back((copies.last_copy(), key, first));
            }
        }
    }

    /// The live refusal of `key` at `now`.
    fn refusal(&mut self, key: u64, now: Instant) -> Option<&mut Copies> {
        self.expire(now);
        self.refused.get_mut(&key).filter(|c| !c.lapsed(now))
    }

    fn holds_refusal(&mut self, key: u64) -> bool {
        self.refusal(key, Instant::now()).is_some()
    }

    /// The copy of the refused INVITE `key` arriving now, if it is one.
    fn copy(&mut self, key: u64) -> Option<RefusedCopy> {
        let now = Instant::now();
        let copies = self.refusal(key, now)?;
        Some(if copies.copy(now) { RefusedCopy::Answered } else { RefusedCopy::Absorbed })
    }

    fn remember(&mut self, key: u64) -> bool {
        let now = Instant::now();
        let first = self.refusal(key, now).is_none();
        if first {
            self.refused.insert(key, Copies::first(Class::InviteClient, now));
            self.order.push_back((now, key, now));
        }
        self.expire(now);
        first
    }

    fn hold(&mut self, key: u64) {
        *self.held.entry(key).or_insert(0) += 1;
    }
}

impl InviteRefusals {
    /// Refusals answered by `answer`, remembering at most
    /// [`REFUSED_MEMO_MAX`] refused identities.
    pub fn new(answer: NewCallRefusal) -> Self {
        Self::with_capacity(answer, REFUSED_MEMO_MAX)
    }

    /// Refusals answered by `answer`, remembering at most `max` refused
    /// identities.
    pub fn with_capacity(answer: NewCallRefusal, max: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                answer,
                hasher: RandomState::new(),
                memo: Mutex::new(Memo {
                    refused: HashMap::new(),
                    order: VecDeque::new(),
                    held: HashMap::new(),
                    max,
                }),
                remembering: AtomicBool::new(false),
                absorbed: AtomicU64::new(0),
            }),
        }
    }

    /// Copies of refused INVITEs absorbed past the RFC schedule (ADR-0063
    /// §1), by this layer or a stage ahead of it (counter).
    pub fn copies_absorbed(&self) -> u64 {
        self.shared.absorbed.load(Ordering::Relaxed)
    }

    /// Whether `req` is a copy of an INVITE refused in the last 64·T1 after
    /// its last copy, counted, and what it draws (ADR-0063 §1): the refusal
    /// on Timer A's schedule, nothing past it. `None` for no refused INVITE.
    pub fn refused_copy(&self, req: &SipRequest) -> Option<RefusedCopy> {
        let key = self.key(req);
        let mut memo = self.lock();
        let copy = memo.copy(key);
        self.mirror(&memo);
        drop(memo);
        self.count(copy)
    }

    /// `copy`, counted when it is absorbed.
    fn count(&self, copy: Option<RefusedCopy>) -> Option<RefusedCopy> {
        if copy == Some(RefusedCopy::Absorbed) {
            self.shared.absorbed.fetch_add(1, Ordering::Relaxed);
        }
        copy
    }

    /// The response refusing `req`: the same for every copy of it.
    pub fn answer(&self, req: &SipRequest) -> Result<SipResponse, GenerateError> {
        (self.shared.answer)(req)
    }

    /// Whether a refusal may be remembered, read without the lock. `false`
    /// means none was as of the last judgement.
    pub fn remembers(&self) -> bool {
        self.shared.remembering.load(Ordering::Relaxed)
    }

    /// Refuse the INVITE `req` unless a live server transaction holds it.
    pub fn refuse(&self, req: &SipRequest) -> Verdict {
        let key = self.key(req);
        let mut memo = self.lock();
        if memo.held.contains_key(&key) {
            return Verdict::Held;
        }
        let first = memo.remember(key);
        self.mirror(&memo);
        Verdict::Refused { first }
    }

    /// Whether `req` shares the identity of an INVITE refused in the last
    /// 64·T1: a copy of it, or the ACK to its refusal (RFC 3261 §17.1.1.3).
    pub fn refused(&self, req: &SipRequest) -> bool {
        let key = self.key(req);
        let mut memo = self.lock();
        let refused = memo.holds_refusal(key);
        self.mirror(&memo);
        refused
    }

    /// The transaction layer's judgement of the INVITE `req`, which no
    /// transaction matches: a copy of a refused INVITE is refused again on
    /// the RFC schedule, absorbed past it and counted; otherwise it is
    /// refused when `refuse`, else admitted, and an initial one (no To-tag)
    /// is held until [`release`](Self::release).
    pub(super) fn admit(&self, req: &SipRequest, refuse: bool) -> Admission {
        let key = self.key(req);
        let mut memo = self.lock();
        let admission = if let Some(copy) = memo.copy(key) {
            self.count(Some(copy));
            Admission::Repeat(copy)
        } else if refuse {
            memo.remember(key);
            Admission::Refuse
        } else if req.to().tag().is_none() {
            memo.hold(key);
            Admission::Admit { held: true }
        } else {
            Admission::Admit { held: false }
        };
        self.mirror(&memo);
        admission
    }

    /// Hold the identity (`branch`, `call_id`, `from_tag`) for a server
    /// transaction rebuilt from a record, which no judgement admitted. The
    /// call exists on this node, so any refusal of that identity is cleared.
    pub(super) fn hold(&self, branch: &str, call_id: &str, from_tag: &str) {
        let key = self.identity(branch, call_id, from_tag);
        let mut memo = self.lock();
        memo.refused.remove(&key);
        memo.hold(key);
        self.mirror(&memo);
    }

    /// Release one hold on the identity (`branch`, `call_id`, `from_tag`),
    /// taken by [`admit`](Self::admit) or [`hold`](Self::hold) for a
    /// transaction now leaving.
    pub(super) fn release(&self, branch: &str, call_id: &str, from_tag: &str) {
        let key = self.identity(branch, call_id, from_tag);
        let mut memo = self.lock();
        if let Some(n) = memo.held.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                memo.held.remove(&key);
            }
        }
    }

    /// The key of `req`'s transaction identity. An ACK to a non-2xx final
    /// shares its INVITE's branch, Call-ID and From-tag (RFC 3261 §17.1.1.3).
    fn key(&self, req: &SipRequest) -> u64 {
        self.identity(
            req.top_via().branch().unwrap_or_default(),
            req.call_id().as_str(),
            req.from().tag().unwrap_or_default(),
        )
    }

    /// The memo's key of (`branch`, `call_id`, `from_tag`), the branch hashed
    /// ignoring ASCII case as it compares (RFC 3261 §7.3.1).
    fn identity(&self, branch: &str, call_id: &str, from_tag: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut state = self.shared.hasher.build_hasher();
        sip_message::header::hash_branch(branch, &mut state);
        call_id.hash(&mut state);
        from_tag.hash(&mut state);
        state.finish()
    }

    fn lock(&self) -> MutexGuard<'_, Memo> {
        crate::poison::locked(&self.shared.memo, crate::poison::INVITE_REFUSALS_MEMO)
    }

    fn mirror(&self, memo: &Memo) {
        self.shared.remembering.store(!memo.refused.is_empty(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timers::{T1, TIMER_B};
    use sip_message::generators::{generate_response, GenerateResponseOpts};
    use sip_message::{CustomParser, SipMessage, SipParser};
    use std::time::Duration;

    fn invite(branch: &str, to_tag: Option<&str>) -> SipRequest {
        let to = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        let raw = format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>{to}\r\n\
Call-ID: {branch}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn refusals() -> InviteRefusals {
        InviteRefusals::new(Arc::new(|req: &SipRequest| {
            generate_response(req, 503, "Service Unavailable", &GenerateResponseOpts::default())
        }))
    }

    /// The first refusal of an INVITE is its first; a copy is refused again.
    #[tokio::test(start_paused = true)]
    async fn a_refused_invite_is_remembered() {
        let r = refusals();
        assert!(!r.remembers());
        assert_eq!(r.refuse(&invite("z9hG4bK-a", None)), Verdict::Refused { first: true });
        assert!(r.remembers());
        assert_eq!(r.refuse(&invite("z9hG4bK-a", None)), Verdict::Refused { first: false });
        assert!(r.refused(&invite("z9hG4bK-a", None)));
        assert!(!r.refused(&invite("z9hG4bK-b", None)));
    }

    /// An INVITE the layer admitted is held: a stage ahead of it spares its
    /// copies until the transaction leaves.
    #[tokio::test(start_paused = true)]
    async fn an_admitted_invite_is_spared_until_released() {
        let r = refusals();
        let req = invite("z9hG4bK-a", None);
        assert!(matches!(r.admit(&req, false), Admission::Admit { held: true }));
        assert_eq!(r.refuse(&req), Verdict::Held);
        assert!(!r.remembers(), "sparing remembers no refusal");
        r.release("z9hG4bK-a", "z9hG4bK-a@10.0.0.1", "alice-tag");
        assert_eq!(r.refuse(&req), Verdict::Refused { first: true });
    }

    /// An INVITE refused ahead of the layer is refused again by it, whatever
    /// the layer's own ceiling says, and never held.
    #[tokio::test(start_paused = true)]
    async fn the_layer_repeats_a_refusal_made_ahead_of_it() {
        let r = refusals();
        let req = invite("z9hG4bK-a", None);
        r.refuse(&req);
        assert!(matches!(r.admit(&req, false), Admission::Repeat(RefusedCopy::Answered)));
        assert!(matches!(r.admit(&invite("z9hG4bK-b", None), true), Admission::Refuse));
        assert_eq!(r.refuse(&invite("z9hG4bK-b", None)), Verdict::Refused { first: false });
    }

    /// The copies of a refused INVITE draw its refusal for Timer A's
    /// re-sends at once, then one per T1, each stage counting on one record;
    /// the absorbed ones are counted.
    #[tokio::test(start_paused = true)]
    async fn a_refused_invites_copies_draw_it_on_timer_as_schedule() {
        let r = refusals();
        let req = invite("z9hG4bK-a", None);
        r.refuse(&req);
        let drawn: Vec<_> = (0..8).map(|_| r.refused_copy(&req)).collect();
        let answered = drawn.iter().filter(|c| **c == Some(RefusedCopy::Answered)).count();
        assert_eq!(answered, 6, "Timer A's re-sends: {drawn:?}");
        assert!(matches!(r.admit(&req, false), Admission::Repeat(RefusedCopy::Absorbed)));
        assert_eq!(r.copies_absorbed(), 3);
        tokio::time::advance(Duration::from_millis(T1)).await;
        assert_eq!(r.refused_copy(&req), Some(RefusedCopy::Answered), "T1 after");
    }

    /// A refusal copied past 64·T1 of its first arrival draws nothing and is
    /// kept 64·T1 after that copy; its ACK still ends there.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_copied_past_64_t1_is_kept_and_answers_nothing() {
        let r = refusals();
        let req = invite("z9hG4bK-a", None);
        r.refuse(&req);
        tokio::time::advance(Duration::from_millis(TIMER_B - 1)).await;
        assert_eq!(r.refused_copy(&req), Some(RefusedCopy::Answered), "inside 64·T1");
        tokio::time::advance(Duration::from_millis(TIMER_B - 1)).await;
        assert_eq!(r.refused_copy(&req), Some(RefusedCopy::Absorbed), "past it");
        tokio::time::advance(Duration::from_millis(TIMER_B - 1)).await;
        assert!(r.refused(&req), "64·T1 after its last copy, less 1 ms");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(r.refused_copy(&req), None, "forgotten 64·T1 after its last copy");
    }

    /// An INVITE carrying a To-tag is never held: no stage ahead judges one.
    #[tokio::test(start_paused = true)]
    async fn an_in_dialog_invite_is_not_held() {
        let r = refusals();
        let req = invite("z9hG4bK-a", Some("bob-tag"));
        assert!(matches!(r.admit(&req, false), Admission::Admit { held: false }));
    }

    /// Two transactions holding one identity keep it held until both leave.
    #[tokio::test(start_paused = true)]
    async fn a_hold_is_counted() {
        let r = refusals();
        let req = invite("z9hG4bK-a", None);
        r.admit(&req, false);
        r.admit(&req, false);
        r.release("z9hG4bK-a", "z9hG4bK-a@10.0.0.1", "alice-tag");
        assert_eq!(r.refuse(&req), Verdict::Held);
        r.release("z9hG4bK-a", "z9hG4bK-a@10.0.0.1", "alice-tag");
        assert_eq!(r.refuse(&req), Verdict::Refused { first: true });
    }

    /// A refusal is forgotten after 64·T1, and the oldest first past the
    /// capacity: a later copy is judged afresh.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_forgotten_after_64_t1_or_past_capacity() {
        let r = refusals();
        r.refuse(&invite("z9hG4bK-a", None));
        tokio::time::advance(std::time::Duration::from_millis(TIMER_B)).await;
        assert!(!r.refused(&invite("z9hG4bK-a", None)));
        assert!(!r.remembers());

        let small = InviteRefusals::with_capacity(r.shared.answer.clone(), 1);
        small.refuse(&invite("z9hG4bK-a", None));
        small.refuse(&invite("z9hG4bK-b", None));
        assert!(!small.refused(&invite("z9hG4bK-a", None)));
        assert!(small.refused(&invite("z9hG4bK-b", None)));
    }
}
