//! [`TransactionConfig`]: the transaction layer's tunables.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::rng::IdGen;

use super::backlog::DeferredBound;
use super::bound_report::BoundPassed;
use super::call_id_budget::CallIdBudget;
use super::new_call_gate::NewCallGate;
use super::refusals::InviteRefusals;

/// What [`TransactionConfig::on_bound`] calls.
pub type OnBound = Arc<dyn Fn(BoundPassed<'_>) + Send + Sync>;

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
    /// Whether the bound of an initial INVITE (no To-tag) runs from its
    /// consumer's arming ([`TransactionLayer::arm_trying`](crate::TransactionLayer::arm_trying))
    /// rather than from its opening: until the consumer arms it or releases
    /// the 100, the transaction sends nothing and absorbs the INVITE's copies,
    /// so a previous hop told nothing fails the request over on its own
    /// timers. An in-dialog INVITE's bound runs from its opening either way.
    /// `false`, the default, runs every bound from the opening (RFC 3261
    /// §17.2.1); `true` departs from it until the arming (ADR-0070).
    pub consumer_arms_initial_trying: bool,
    /// The bound on the hop-by-hop ACK of a client INVITE's first non-2xx
    /// final, ms: the layer holds the ACK until its consumer releases it
    /// ([`TransactionLayer::release_ack`](crate::TransactionLayer::release_ack))
    /// and sends it itself this long after the final arrived. `0`, the
    /// default, sends it as the final arrives (RFC 3261 §17.1.1.2).
    pub ack_bound_ms: u64,
    /// The bound on the 200 to a CANCEL and the 487 to the INVITE it cancels,
    /// ms: the layer holds both until its consumer releases them
    /// ([`TransactionLayer::release_cancel`](crate::TransactionLayer::release_cancel))
    /// and sends them itself this long after the CANCEL arrived. `0`, the
    /// default, sends them as the CANCEL arrives (RFC 3261 §9.2).
    pub cancel_bound_ms: u64,
    /// Whether a CANCEL of an in-dialog INVITE (a re-INVITE) leaves that
    /// INVITE's final to the consumer, which may relay the CANCEL on and
    /// answer with the final the CANCEL draws there (RFC 3261 §9.2, §16.10).
    /// The layer then holds the CANCEL's 200 and the 487 it would answer, as
    /// under [`Self::cancel_bound_ms`] (its own bound being the §17.2.1 bound
    /// where that one is `0`), and the consumer's release says whether the 487
    /// leaves ([`CancelledInvite`](crate::CancelledInvite)). At the bound only
    /// the 200 leaves; a release that never comes ends the INVITE with the 487
    /// 64·T1 past the CANCEL. `false`, the default, answers 487 as the CANCEL
    /// arrives.
    pub consumer_ends_cancelled_reinvite: bool,
    /// Told each time a held ACK, or the held answers to a CANCEL, leave at
    /// their bound rather than on their consumer's release, with the
    /// transaction that held them.
    pub on_bound: Option<OnBound>,
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
    /// The consumer's switch on new calls and the age of its queue
    /// ([`NewCallGate`]), read as each new INVITE arrives; it refuses through
    /// [`invite_refusals`](Self::invite_refusals), which it requires. `None`
    /// — the default — reads none.
    pub new_call_gate: Option<NewCallGate>,
    /// Whether an out-of-dialog OPTIONS (no To-tag) opens no server
    /// transaction: every copy reaches the consumer as a `Message`, and the
    /// answer it sends leaves raw, so the consumer derives its To-tag from
    /// the request (RFC 3261 §8.2.7). `false` — the default — holds one
    /// transaction per OPTIONS for 64·T1 like any other non-INVITE.
    pub stateless_options: bool,
    /// The bound of the keepalive lane, which carries every out-of-dialog
    /// OPTIONS [`stateless_options`](Self::stateless_options) passes up, apart
    /// from the event queue: a consumer that takes the lane first answers a
    /// probe ahead of the events queued before it. A full lane drops the
    /// newest and counts it; the prober repeats. `None` — the default — runs
    /// no lane: every OPTIONS rides the event queue.
    pub keepalive_lane: Option<usize>,
    /// The bound of the unattributed lane, which carries, apart from the
    /// event queue, every request that matches no transaction here and whose
    /// Request-URI states no `callRef`, other than an initial INVITE (no
    /// To-tag), a CANCEL and an OPTIONS on the keepalive lane: a request that
    /// can belong to no call this element names, or to one whose Request-URI
    /// an element on its path rewrote. The `Cancelled` of an INVITE such a
    /// request opened rides it too, behind that INVITE. A request that finds the lane
    /// full is dropped before any transaction, answer or record is made, and
    /// counted: its sender's retransmission is judged afresh. A consumer that
    /// takes the lane after the event queue keeps a flood of such requests
    /// from taking the room, and the age, the requests of its calls need.
    /// `None` — the default — runs no lane: they ride the event queue.
    pub unattributed_lane: Option<usize>,
    /// The address every request the layer builds itself leaves for, whatever
    /// its Route or Request-URI says: the front proxy's, for an element behind
    /// one. `None`, the default: such a request goes to its own next hop.
    pub first_hop: Option<SocketAddr>,
    /// The `hostport` this element is advertised as, which names it as the
    /// warn-agent of the 400 answering a refused request (RFC 3261 §20.43).
    /// `None` — the default — names it by the address the 400 leaves from.
    pub advertised: Option<String>,
    /// The message budget of each Call-ID (ADR-0063 §3, [`CallIdBudget`]):
    /// every datagram received that matches a transaction or draws an
    /// answer, and every datagram sent, raw sends included, counts under its
    /// Call-ID, the keepalive lane and the 400 refusing a parse aside; past
    /// the budget the Call-ID is muted and
    /// [`TransactionEvent::CallIdBudgetTripped`](crate::TransactionEvent)
    /// told. `None` — the default — counts nothing.
    pub call_id_budget: Option<CallIdBudget>,
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
    /// bound, no shared refusals, no new-call gate, a transaction per OPTIONS, no keepalive
    /// lane, no unattributed lane, no first hop, no advertised address, no Call-ID budget.
    pub fn new(id_gen: Arc<IdGen>) -> Self {
        Self {
            udp_queue_max: 256,
            id_gen,
            invite_initial_timeout_ms: crate::timers::INVITE_INITIAL_TIMEOUT,
            invite_first_response_timeout_ms: crate::timers::TIMER_B,
            trying_bound_ms: crate::timers::TRYING_BOUND,
            consumer_arms_initial_trying: false,
            ack_bound_ms: 0,
            cancel_bound_ms: 0,
            consumer_ends_cancelled_reinvite: false,
            on_bound: None,
            strict_to_tag: true,
            deferred_bound: None,
            invite_refusals: None,
            new_call_gate: None,
            stateless_options: false,
            keepalive_lane: None,
            unattributed_lane: None,
            first_hop: None,
            advertised: None,
            call_id_budget: None,
        }
    }
}
