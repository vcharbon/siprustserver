//! [`TransactionConfig`]: the transaction layer's tunables.

use std::sync::Arc;

use crate::rng::IdGen;

use super::backlog::DeferredBound;
use super::refusals::InviteRefusals;

/// Tunables for the transaction layer.
pub struct TransactionConfig {
    /// The network layer's recv-queue bound. The output event queue is sized
    /// from it ([`event_capacity`](Self::event_capacity)).
    pub udp_queue_max: usize,
    /// Identifier seam (Via branch / To-tag generation).
    pub id_gen: Arc<IdGen>,
    /// The INVITE transaction bound once a provisional has arrived, ms — the
    /// client txn's give-up timer for an INVITE in Proceeding (initial or
    /// in-dialog; before any response it is on Timer B) AND the backstop of
    /// every active INVITE transaction, either role, derive from this one
    /// value, so both halves of a call admit the same ring window. Default
    /// [`INVITE_INITIAL_TIMEOUT`](crate::timers::INVITE_INITIAL_TIMEOUT)
    /// (158 s); the consumer validates its own range and MUST keep every
    /// app-level setup deadline strictly below it, or the txn layer CANCELs
    /// the callee before the app gives up.
    pub invite_initial_timeout_ms: u64,
    /// The TU's first-response deadline on an initial INVITE (no To-tag), ms
    /// (ADR-0044): with no response of any kind by then, the TU is told the
    /// hop is dead (`TxnTimeoutKind::Response`) while the transaction keeps Timer
    /// A to Timer B and reports nothing again. Armed only below Timer B;
    /// default [`TIMER_B`](crate::timers::TIMER_B), which arms nothing. An
    /// in-dialog INVITE and a non-INVITE never read it.
    pub invite_first_response_timeout_ms: u64,
    /// The §17.2.1 bound on an INVITE's first provisional, ms: a server INVITE
    /// transaction whose consumer has sent nothing and released no 100
    /// ([`TransactionLayer::release_trying`](crate::TransactionLayer::release_trying))
    /// this long after its INVITE arrived sends the 100 itself. Default
    /// [`TRYING_BOUND`](crate::timers::TRYING_BOUND); the RFC allows no more.
    pub trying_bound_ms: u64,
    /// Whether a response the TU hands over under a To-tag other than the
    /// bound one fails a debug build loudly (`debug_assert!`) besides being
    /// re-rendered. The wire is corrected either way; `true` — the default —
    /// makes the defect a test failure, and a test that exercises the
    /// correction itself turns it off.
    pub strict_to_tag: bool,
    /// The ceilings on the deferred backlog past which an INVITE no
    /// transaction holds is refused ([`DeferredBound`]), with
    /// [`invite_refusals`](Self::invite_refusals)' answer, which it requires.
    /// `None` — the default — sets none: the backlog then grows with the
    /// INVITEs admitted while the consumer does not drain, until their
    /// transactions' backstops remove them.
    pub deferred_bound: Option<DeferredBound>,
    /// The node's refusals of new INVITEs, shared with any stage ahead of this
    /// layer that refuses one ([`InviteRefusals`]): a copy of an INVITE they
    /// hold refused draws that refusal here, before any transaction exists,
    /// and an initial INVITE this layer admits is held there while its
    /// transaction lives. `None` — the default — shares nothing.
    pub invite_refusals: Option<InviteRefusals>,
    /// Whether an out-of-dialog OPTIONS (no To-tag) opens no server
    /// transaction: every copy reaches the consumer as a `Message`, and the
    /// answer it sends leaves raw, so the consumer derives its To-tag from
    /// the request (RFC 3261 §8.2.7). `false` — the default — holds one
    /// transaction per OPTIONS for 64·T1 like any other non-INVITE.
    pub stateless_options: bool,
    /// The `hostport` this element is advertised as, which names it as the
    /// warn-agent of the 400 answering a refused request (RFC 3261 §20.43).
    /// `None` — the default — names it by the address the 400 leaves from.
    pub advertised: Option<String>,
}

/// The output event queue's floor and ceiling.
const EVENT_CAPACITY: std::ops::RangeInclusive<usize> = 64..=1 << 20;

impl TransactionConfig {
    /// The output event queue's bound: `udp_queue_max * 4`, at least 64 and
    /// at most 2^20.
    pub fn event_capacity(&self) -> usize {
        self.udp_queue_max.saturating_mul(4).clamp(*EVENT_CAPACITY.start(), *EVENT_CAPACITY.end())
    }

    /// The defaults, identifiers drawn from `id_gen`: a 256-datagram receive
    /// queue, the RFC 3261 INVITE bounds and first-provisional bound, strict To-tag binding, no deferred
    /// bound, no shared refusals, a transaction per OPTIONS, no advertised
    /// address.
    pub fn new(id_gen: Arc<IdGen>) -> Self {
        Self {
            udp_queue_max: 256,
            id_gen,
            invite_initial_timeout_ms: crate::timers::INVITE_INITIAL_TIMEOUT,
            invite_first_response_timeout_ms: crate::timers::TIMER_B,
            trying_bound_ms: crate::timers::TRYING_BOUND,
            strict_to_tag: true,
            deferred_bound: None,
            invite_refusals: None,
            stateless_options: false,
            advertised: None,
        }
    }
}
