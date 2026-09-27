//! Admission against the deferred backlog (ADR-0037 item 6): a new INVITE
//! arriving while the backlog holds its class's ceiling is refused
//! statelessly, before the 100 Trying and before any transaction exists, so
//! the retry deque stays bounded while the consumer is not draining the
//! output queue. The refused identities are remembered for 64·T1, so every
//! later copy of one INVITE draws the same refusal and its ACK ends here.

use std::collections::hash_map::RandomState;
use std::collections::{HashSet, VecDeque};
use std::hash::BuildHasher;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use sip_message::emergency::is_emergency_request;
use sip_message::{SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use crate::timers::{ms, TIMER_B};

use super::owner::Owner;

/// Builds the response that refuses an INVITE at a ceiling. The consumer
/// phrases it (status, To-tag, `Retry-After`); the layer only sends it, and
/// sends it again for every later copy of that INVITE, so it must answer one
/// request identically each time (RFC 3261 §8.2.7).
pub type NewCallRefusal = Arc<dyn Fn(&SipRequest) -> SipResponse + Send + Sync>;

/// Ceilings on the deferred backlog (the critical events a full output queue
/// holds for later), in events. A CANCEL, an ACK, a response and every event
/// the layer emits for a transaction it already holds are never refused; a
/// retransmission of an admitted INVITE matches its transaction first and is
/// never judged.
#[derive(Clone)]
pub struct DeferredBound {
    /// The ceiling for a new non-emergency initial INVITE.
    pub normal: usize,
    /// The ceiling for an emergency initial INVITE (RFC 4412
    /// `Resource-Priority`) and for an INVITE carrying a To-tag: this layer
    /// cannot tell a dialog the consumer holds from one it never had, and a
    /// 503 leaves an existing dialog in place (RFC 3261 §12.2.1.2, §14.1).
    /// Read as at least `normal`.
    pub emergency: usize,
    /// The response a refused INVITE draws.
    pub refusal: NewCallRefusal,
}

/// Why an INVITE was refused at a ceiling: the `class` of the refusal counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusedClass {
    /// A new initial INVITE, at the normal ceiling.
    Normal,
    /// A new emergency initial INVITE, at the emergency ceiling.
    Emergency,
    /// An INVITE carrying a To-tag, at the emergency ceiling.
    InDialog,
}

impl RefusedClass {
    pub const ALL: [RefusedClass; 3] = [Self::Normal, Self::Emergency, Self::InDialog];

    /// Stable metric label (`class="..."`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Emergency => "emergency",
            Self::InDialog => "in_dialog",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Emergency => 1,
            Self::InDialog => 2,
        }
    }
}

/// The most refused identities remembered at once; past it the oldest is
/// forgotten first, and a later copy of that INVITE is judged afresh.
const REFUSED_MEMO_MAX: usize = 65_536;

/// The identities (top-`Via` branch, Call-ID, From-tag) of the INVITEs
/// refused in the last 64·T1 (Timer B, the longest a UAC retransmits an
/// INVITE), as keyed hashes with their refusal instant, oldest first.
pub(super) struct RefusedMemo {
    keys: HashSet<u64>,
    order: VecDeque<(Instant, u64)>,
    hasher: RandomState,
}

impl RefusedMemo {
    pub(super) fn new() -> Self {
        Self { keys: HashSet::new(), order: VecDeque::new(), hasher: RandomState::new() }
    }

    /// The key of `req`'s transaction identity. An ACK to a non-2xx final
    /// shares its INVITE's branch, Call-ID and From-tag (RFC 3261 §17.1.1.3).
    fn key(&self, req: &SipRequest) -> u64 {
        self.hasher.hash_one((
            req.top_via().branch().unwrap_or_default(),
            req.call_id().as_str(),
            req.from().tag().unwrap_or_default(),
        ))
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(at, key)) = self.order.front() {
            if now.duration_since(at) < ms(TIMER_B) && self.order.len() <= REFUSED_MEMO_MAX {
                break;
            }
            self.order.pop_front();
            self.keys.remove(&key);
        }
    }

    fn remember(&mut self, req: &SipRequest) {
        let now = Instant::now();
        let key = self.key(req);
        if self.keys.insert(key) {
            self.order.push_back((now, key));
        }
        self.expire(now);
    }

    fn holds(&mut self, req: &SipRequest) -> bool {
        self.expire(Instant::now());
        !self.keys.is_empty() && self.keys.contains(&self.key(req))
    }
}

impl Owner {
    /// Refuse the INVITE `req`, which no transaction holds, when the backlog
    /// already holds its class's ceiling: the refusal goes to `src`, the
    /// identity is remembered and the refusal counted. `true` when refused —
    /// the caller creates no transaction and sends no 100 Trying.
    pub(super) async fn refuse_on_backlog(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> bool {
        let Some(bound) = &self.deferred_bound else { return false };
        let backlog = self.deferred_events.len();
        // Below the normal ceiling no class is refused, so every ceiling reads
        // as at least `normal`.
        if backlog < bound.normal {
            return false;
        }
        let class = if req.to().tag().is_some() {
            RefusedClass::InDialog
        } else if is_emergency_request(req) {
            RefusedClass::Emergency
        } else {
            RefusedClass::Normal
        };
        if class != RefusedClass::Normal && backlog < bound.emergency {
            return false;
        }
        let refusal = (bound.refusal)(req);
        self.send_buffer(endpoint, refusal.image(), src).await;
        self.refused.remember(req);
        self.metrics.deferred_refused[class.index()].fetch_add(1, Relaxed);
        true
    }

    /// A copy of an INVITE refused in the last 64·T1 draws that refusal again,
    /// whatever the backlog holds now: the caller has its final, or will have
    /// it from this copy, and a call admitted behind it would ring for no one.
    /// `true` when `req` was such a copy.
    pub(super) async fn repeat_refusal(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> bool {
        let Some(bound) = &self.deferred_bound else { return false };
        if !self.refused.holds(req) {
            return false;
        }
        let refusal = (bound.refusal)(req);
        self.send_buffer(endpoint, refusal.image(), src).await;
        true
    }

    /// Whether `ack`, matching no transaction, acknowledges a refusal of the
    /// last 64·T1: it ends here, as a transaction's ACK to its non-2xx final
    /// would (RFC 3261 §17.2.1).
    pub(super) fn acknowledges_refusal(&mut self, ack: &SipRequest) -> bool {
        self.deferred_bound.is_some() && self.refused.holds(ack)
    }
}
