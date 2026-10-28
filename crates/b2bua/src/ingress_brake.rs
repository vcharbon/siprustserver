//! The ingress brake: the arrival-time [`PreIngressHook`] installed on the
//! worker's UDP bind, the first rung of the admission ladder
//! ([`crate::admission`]).
//!
//! Its one goal is to **refuse new normal calls when the ingress queue is
//! saturated**, before the datagram is queued and before any transaction or
//! call state exists. Below the threshold, and while the worker remembers no
//! refused INVITE, it is an integer compare and an atomic load, and the packet
//! is accepted unparsed (the transaction layer parses it).
//!
//! At or above `floor(queue_max * threshold_pct / 100)` the datagram is
//! parsed, and the parse is queued with it: the transaction layer takes it
//! and does not parse the datagram again ([`sip_net::Arrival`]). Then:
//!
//!   - a datagram the parser refuses → accept (the transaction layer answers
//!     or drops it);
//!   - a response, any request but an INVITE → accept;
//!   - a copy of an initial INVITE the worker refused in the last 64·T1 — at
//!     this brake or at the transaction layer's deferred backlog, whatever
//!     its class → that refusal again, counted as a copy;
//!   - otherwise the ladder's brake rung judges it in its class
//!     ([`class_of`]): an INVITE carrying a `To`-tag is admitted (the brake
//!     never touches an existing call), an emergency one too, counted on
//!     [`IngressBrakeCounters::emergency_bypassed`]; a normal one is refused
//!     with the worker's one refusal ([`Refusals`]) unless a live server
//!     transaction holds a copy of it (that transaction answers it), its
//!     first refusal counted as a new call refused in its class.
//!
//! Below the threshold, while the worker remembers a refused INVITE, the
//! datagram is parsed the same way, and an initial INVITE the worker refused
//! in the last 64·T1 — at this brake or at the transaction layer's deferred
//! backlog — draws that refusal again, so a refused call is never admitted
//! behind its caller's back. Every other datagram below the threshold is
//! accepted.
//!
//! The brake shares the worker's one refusal memo and answer with the
//! transaction layer: every copy of one INVITE draws the same bytes, the
//! stateless-UAS rule of RFC 3261 §8.2.7, whichever stage refused it.
//!
//! Counters are `Arc<AtomicU64>` because a [`PreIngressHook`] is an immutable
//! `Fn` shared across the recv task(s); the read side stays lock-free for the
//! `/metrics` scrape. The memo's lock is taken only on an INVITE above the
//! threshold or, while a refusal is remembered, on an INVITE below it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_message::received::{stamp_received, Received};
use sip_message::{serialize, CustomParser, Method, SipMessage, SipRequest};
use sip_net::types::{PreIngressAction, PreIngressHook};
use sip_net::Arrival;
use sip_txn::{InviteRefusals, Verdict as Memo};

use crate::admission::{class_of, judge, Class, Refusals, Rung, Verdict};
use crate::new_calls::StatelessCounts;

/// Tunables for the ingress brake. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngressBrakeConfig {
    /// The bound on the inbound queue this brake fronts. The brake's
    /// [`threshold`](Self::threshold) is a percentage of it.
    pub queue_max: usize,
    /// Activation threshold as a **percent** of [`queue_max`](Self::queue_max).
    /// The brake engages once the live queue depth reaches
    /// `floor(queue_max * pct / 100)`.
    pub threshold_pct: u32,
}

impl IngressBrakeConfig {
    /// The absolute queue depth at/above which the brake engages:
    /// `floor(queue_max * threshold_pct / 100)`. `u64` widens the product
    /// before the floor-divide so no realistic queue bound can overflow.
    pub fn threshold(&self) -> usize {
        let product = self.queue_max as u64 * u64::from(self.threshold_pct);
        (product / 100) as usize
    }

    /// The most refused identities the worker's memo should hold behind this
    /// brake: a full queue of INVITEs every second for 64·T1, and never fewer
    /// than [`sip_txn::REFUSED_MEMO_MAX`].
    pub fn memo_capacity(&self) -> usize {
        self.queue_max.saturating_mul(32).max(sip_txn::REFUSED_MEMO_MAX)
    }
}

/// The brake's observability surface, as shareable lock-free atomics. One
/// instance is captured by the [`PreIngressHook`] (write side) and retained by
/// the runner (read side, for the `/metrics` scrape). Clone shares the same
/// atomics.
#[derive(Debug, Clone, Default)]
pub struct IngressBrakeCounters {
    /// Initial emergency INVITEs that crossed the threshold and were admitted
    /// anyway — non-zero under flood means the brake is correctly letting
    /// emergency calls through while refusing the rest.
    emergency_bypassed: Arc<AtomicU64>,
    /// INVITEs refused here first, by [`Class`].
    refused: Arc<[AtomicU64; 3]>,
    /// Copies of a refused INVITE refused again here.
    copies: Arc<AtomicU64>,
}

impl IngressBrakeCounters {
    /// Fresh counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Initial emergency INVITEs that crossed the threshold and bypassed the
    /// brake.
    pub fn emergency_bypassed(&self) -> u64 {
        self.emergency_bypassed.load(Ordering::Relaxed)
    }

    /// Record one emergency INVITE that crossed the threshold but bypassed the
    /// brake.
    pub fn record_emergency_bypass(&self) {
        self.emergency_bypassed.fetch_add(1, Ordering::Relaxed);
    }

    /// INVITEs of `class` this brake refused first: each once, however many
    /// of its copies were answered.
    pub fn refused(&self, class: Class) -> u64 {
        self.refused.get(class.index()).map_or(0, |n| n.load(Ordering::Relaxed))
    }

    /// Copies of a refused INVITE — refused here or at the backlog — this
    /// brake answered with that refusal again.
    pub fn refused_copies(&self) -> u64 {
        self.copies.load(Ordering::Relaxed)
    }

    /// The brake's first refusals and copies, for the new-call count
    /// ([`crate::new_calls::NewCallCounts::read`]).
    pub fn counts(&self) -> StatelessCounts {
        StatelessCounts {
            refused: Class::ALL.map(|c| self.refused(c)),
            copies: self.refused_copies(),
        }
    }

    fn record_refused(&self, class: Class) {
        if let Some(n) = self.refused.get(class.index()) {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_copy(&self) {
        self.copies.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build the ingress brake [`PreIngressHook`] for the worker's UDP bind.
///
/// The returned closure runs at arrival time for every datagram, with the live
/// inbound-queue `depth`, and applies the classification in the module doc.
/// `refusals` is the worker's one instance, whose memo the transaction layer
/// holds too ([`sip_txn::TransactionConfig::invite_refusals`]).
pub fn build_ingress_brake_hook(
    config: IngressBrakeConfig,
    counters: IngressBrakeCounters,
    refusals: Refusals,
) -> PreIngressHook {
    let threshold = config.threshold();
    let parser = CustomParser::new();
    let memo = refusals.memo().clone();
    Arc::new(move |arrival: &mut Arrival, src: SocketAddr, depth: usize| {
        let above = depth >= threshold;
        if !above && !memo.remembers() {
            return PreIngressAction::Accept;
        }
        // What the parser refuses is the pipeline's to answer, and responses
        // and other methods are always admitted: each is queued with its parse.
        let Ok(Ok(SipMessage::Request(req))) = arrival.parse(&parser) else {
            return PreIngressAction::Accept;
        };
        if *req.method() != Method::Invite {
            return PreIngressAction::Accept;
        }
        let class = class_of(req);
        // A copy of an INVITE the worker refused draws that refusal again, on
        // either side of the threshold and whatever its class. A held identity
        // is never refused (see `InviteRefusals`), so a live call is spared.
        if class != Class::InDialog && memo.remembers() && memo.refused(req) {
            counters.record_copy();
            return reply(&memo, req, src);
        }
        match judge(Rung::Brake { depth, threshold }, class) {
            Verdict::Admit => {
                if above && class == Class::Emergency {
                    counters.record_emergency_bypass();
                }
                PreIngressAction::Accept
            }
            // The reply is built before the refusal is remembered: a reply
            // that cannot be built leaves no memo entry the transaction
            // layer would rebuild that same answer from.
            Verdict::Refuse(_) => {
                let reply = reply(&memo, req, src);
                match memo.refuse(req) {
                    Memo::Held => PreIngressAction::Accept,
                    Memo::Refused { first } => {
                        if first {
                            counters.record_refused(class);
                        } else {
                            counters.record_copy();
                        }
                        reply
                    }
                }
            }
        }
    })
}

/// The worker's refusal of `req`, received from `src`, as the datagram the
/// hook sends back: composed from the request as the transaction layer reads
/// it, its top Via stamped on receipt (RFC 3261 §18.2.1, RFC 3581 §4), and
/// sent to that Via's response target (§18.2.2), as the transaction layer
/// sends the refusal of any copy it meets. A refused INVITE whose lines no
/// response can echo is dropped.
fn reply(memo: &InviteRefusals, req: &SipRequest, src: SocketAddr) -> PreIngressAction {
    let Received { request, destination: to, .. } = stamp_received(req.clone(), src);
    match memo.answer(&request) {
        Ok(refusal) => {
            PreIngressAction::Reply { datagram: serialize(&SipMessage::Response(refusal)), to }
        }
        Err(_) => PreIngressAction::Drop,
    }
}

#[cfg(test)]
mod tests;
