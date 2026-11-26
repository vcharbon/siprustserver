//! The refusal memos' storage: striped maps of the requests the proxy
//! answered itself, keyed by their received transaction, each entry counting
//! its copies on the sender's schedule (ADR-0063 §1) and forgotten 64·T1 past
//! its last copy or past its stripe's capacity.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use sip_message::Method;
use sip_retransmit::Class;
use sip_txn::timers::TIMER_B;

use super::Answer;

/// The memo's independently locked stripes. A copy hashes to the stripe its
/// original went to, so striping changes no answer; it bounds the requests
/// that contend on one lock to those whose keys share a stripe.
pub(super) const STRIPES: usize = 16;

/// One memo, striped: each stripe's mutex guards that stripe's entries; a
/// lookup takes one stripe's lock, and none while the memo holds nothing
/// (read from a lock-free count).
pub(super) struct Striped {
    stripes: [Mutex<Memo>; STRIPES],
    remembered: AtomicUsize,
}

/// One refusal: the copies of its request, those of a CANCEL of it, the
/// final it drew (`None` for a preflight reject, which its check composes
/// again) and the To-tag it was answered with.
pub(super) struct Refused {
    pub(super) copies: Copies,
    pub(super) cancel: Option<Copies>,
    pub(super) answer: Option<Answer>,
    pub(super) to_tag: Box<str>,
}

/// The copies of one request the proxy answered itself (ADR-0063 §1): when
/// it first arrived, how many copies followed, when the last one arrived and
/// when the last answered one did, in ms.
#[derive(Clone, Copy)]
pub(super) struct Copies {
    class: Class,
    first_ms: u64,
    copies: u32,
    last_ms: u64,
    last_answered_ms: u64,
}

impl Copies {
    /// The first arrival at `now_ms` of a request of `method`, which its
    /// sender re-sends on Timer A (an INVITE) or Timer E.
    pub(super) fn first(method: &Method, now_ms: u64) -> Self {
        let class = match method {
            Method::Invite => Class::InviteClient,
            Method::Cancel => Class::CancelClient,
            _ => Class::NonInviteClient,
        };
        Self { class, first_ms: now_ms, copies: 0, last_ms: now_ms, last_answered_ms: now_ms }
    }

    /// Count a copy arriving at `now_ms`; whether it is answered: inside the
    /// 64·T1 of the first arrival and owed on the sender's ladder.
    pub(super) fn copy(&mut self, now_ms: u64) -> bool {
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
    pub(super) fn lapsed(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_ms) >= TIMER_B
    }
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

impl Refused {
    pub(super) fn first(
        method: &Method,
        now_ms: u64,
        answer: Option<Answer>,
        to_tag: Box<str>,
    ) -> Self {
        Self { copies: Copies::first(method, now_ms), cancel: None, answer, to_tag }
    }
}

impl Striped {
    /// An empty memo of about `max` entries: each stripe holds
    /// `max / STRIPES`, rounded up.
    pub(super) fn new(max: usize) -> Self {
        let per_stripe = max.div_ceil(STRIPES).max(1);
        Self {
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

    /// Remember `refused` under `key` at `now_ms`, its first arrival.
    pub(super) fn note(&self, key: u64, refused: Refused, now_ms: u64) {
        let mut memo = self.lock(key);
        let added = usize::from(memo.refused.insert(key, refused).is_none());
        memo.order.push_back((now_ms, key, now_ms));
        let forgotten = memo.expire(now_ms);
        self.count(added, forgotten);
    }

    /// `read` of the entry under `key`, not lapsed at `now_ms`.
    pub(super) fn with<T>(
        &self,
        key: u64,
        now_ms: u64,
        read: impl FnOnce(&mut Refused) -> T,
    ) -> Option<T> {
        if self.remembered.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let mut memo = self.lock(key);
        let forgotten = memo.expire(now_ms);
        self.count(0, forgotten);
        memo.refused.get_mut(&key).filter(|r| !r.copies.lapsed(now_ms)).map(read)
    }

    /// The stripe of `key`, locked: STRIPES divides 256, so the key's low
    /// byte picks the stripe its remainder by STRIPES names.
    fn lock(&self, key: u64) -> MutexGuard<'_, Memo> {
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
