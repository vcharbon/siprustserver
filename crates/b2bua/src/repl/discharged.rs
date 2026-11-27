//! The identities of the calls a peer discharged because a poll of their turn
//! never returned (ADR-0057 §6). The router refuses a new INVITE on one of
//! them (Call-ID and From-tag) for 64·T1 after the discharge's delete applied
//! here: a retry or a copy of the INVITE that stuck the peer never reaches
//! this node's routing consult.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::store::{locked, DISCHARGED_IDENTITIES};

/// How long an identity stays refused after its discharge applied here:
/// 64·T1 (Timer B), the longest the previous hop keeps the INVITE's
/// transaction and its copies going.
pub const DISCHARGED_WINDOW: Duration = Duration::from_millis(sip_retransmit::timers::TIMER_B);

/// The most identities held. A worker discharges one call per stuck poll and
/// exits, so a node meets a handful; past the bound the oldest is forgotten
/// first, and its identity is served again.
pub const DISCHARGED_MAX: usize = 1_024;

/// The refused identities, each until its window ends. Clone shares them.
///
/// The lookup reads a lock-free mirror first: a node holding none takes no
/// lock on a new INVITE.
#[derive(Clone)]
pub struct DischargedIdentities {
    shared: Arc<Shared>,
}

struct Shared {
    hasher: RandomState,
    max: usize,
    held: Mutex<Held>,
    any: AtomicBool,
}

/// The identities as keyed hashes, each with the instant its window ends,
/// and the same keys in the order their windows end, each once.
#[derive(Default)]
struct Held {
    until: HashMap<u64, Instant>,
    order: VecDeque<u64>,
}

impl Held {
    /// Forget every identity whose window ended at `now`, and the oldest
    /// past `max`.
    fn expire(&mut self, now: Instant, max: usize) {
        while let Some(&key) = self.order.front() {
            let ended = self.until.get(&key).is_none_or(|until| *until <= now);
            if !ended && self.order.len() <= max {
                break;
            }
            self.order.pop_front();
            self.until.remove(&key);
        }
    }
}

impl Default for DischargedIdentities {
    fn default() -> Self {
        Self::with_capacity(DISCHARGED_MAX)
    }
}

impl DischargedIdentities {
    /// Holding at most `max` identities.
    pub fn with_capacity(max: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                hasher: RandomState::new(),
                max,
                held: Mutex::new(Held::default()),
                any: AtomicBool::new(false),
            }),
        }
    }

    /// Refuse the identity of the call `call_ref` names from now until
    /// [`DISCHARGED_WINDOW`] has passed; a ref naming no identity refuses
    /// nothing. A second discharge of an identity restarts its window.
    pub fn refuse(&self, call_ref: &str) {
        let Some(parsed) = call::parse_call_ref(call_ref) else {
            return;
        };
        let key = self.key(&parsed.call_id, &parsed.from_tag);
        let now = Instant::now();
        let Some(until) = now.checked_add(DISCHARGED_WINDOW) else {
            return;
        };
        let mut held = locked(&self.shared.held, DISCHARGED_IDENTITIES);
        if held.until.insert(key, until).is_some() {
            held.order.retain(|k| *k != key);
        }
        held.order.push_back(key);
        held.expire(now, self.shared.max);
        self.shared.any.store(!held.until.is_empty(), Ordering::Relaxed);
    }

    /// Whether a new INVITE on `call_id` and `from_tag` is refused now.
    pub fn refuses(&self, call_id: &str, from_tag: &str) -> bool {
        if !self.shared.any.load(Ordering::Relaxed) {
            return false;
        }
        let key = self.key(call_id, from_tag);
        let mut held = locked(&self.shared.held, DISCHARGED_IDENTITIES);
        held.expire(Instant::now(), self.shared.max);
        self.shared.any.store(!held.until.is_empty(), Ordering::Relaxed);
        held.until.contains_key(&key)
    }

    /// The identities held, expired ones included until a lookup forgets
    /// them.
    pub fn len(&self) -> usize {
        locked(&self.shared.held, DISCHARGED_IDENTITIES).until.len()
    }

    /// Whether no identity is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn key(&self, call_id: &str, from_tag: &str) -> u64 {
        let mut state = self.shared.hasher.build_hasher();
        call_id.hash(&mut state);
        from_tag.hash(&mut state);
        state.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_ref(call_id: &str) -> String {
        call::derive_call_ref("w1", call_id, "tag-a")
    }

    /// Refused from the discharge until 64·T1 has passed, on its Call-ID and
    /// From-tag whatever primary placed the retry; never another identity.
    #[tokio::test(start_paused = true)]
    async fn an_identity_is_refused_for_64_t1_from_its_discharge() {
        let set = DischargedIdentities::default();
        assert!(!set.refuses("a@host", "tag-a"));
        set.refuse(&call_ref("a@host"));
        assert!(set.refuses("a@host", "tag-a"));
        assert!(!set.refuses("a@host", "tag-b"), "another From-tag");
        assert!(!set.refuses("b@host", "tag-a"), "another Call-ID");
        tokio::time::advance(DISCHARGED_WINDOW - Duration::from_millis(1)).await;
        assert!(set.refuses("a@host", "tag-a"), "1 ms before the window ends");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!set.refuses("a@host", "tag-a"), "the window ended");
        assert!(set.is_empty(), "the lookup forgot it");
    }

    /// A lookup inside the window never stretches it.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_does_not_extend_the_window() {
        let set = DischargedIdentities::default();
        set.refuse(&call_ref("a@host"));
        for _ in 0..4 {
            tokio::time::advance(DISCHARGED_WINDOW / 4 - Duration::from_millis(1)).await;
            assert!(set.refuses("a@host", "tag-a"));
        }
        tokio::time::advance(Duration::from_millis(4)).await;
        assert!(!set.refuses("a@host", "tag-a"));
    }

    /// Past the bound the oldest identity is forgotten first; a second
    /// discharge of an identity makes it the newest.
    #[tokio::test(start_paused = true)]
    async fn past_the_bound_the_oldest_identity_goes_first() {
        let set = DischargedIdentities::with_capacity(2);
        set.refuse(&call_ref("a@host"));
        set.refuse(&call_ref("b@host"));
        set.refuse(&call_ref("a@host"));
        set.refuse(&call_ref("c@host"));
        assert_eq!(set.len(), 2);
        assert!(!set.refuses("b@host", "tag-a"), "the oldest went");
        assert!(set.refuses("a@host", "tag-a"), "refused again, the newest but one");
        assert!(set.refuses("c@host", "tag-a"));
    }

    /// A ref naming no identity refuses nothing.
    #[tokio::test(start_paused = true)]
    async fn a_ref_naming_no_identity_refuses_nothing() {
        let set = DischargedIdentities::default();
        set.refuse("no-separator");
        assert!(set.is_empty());
    }
}
