//! Every datagram the owner receives or sends, held to its Call-ID's message
//! budget (ADR-0063 §3, §4): counted, or dropped under a mute. The trip that
//! mutes a Call-ID is counted, logged and told to the consumer on the
//! lossless path. Out-of-dialog OPTIONS on the keepalive lane and the
//! stateless 400 of a refused parse are outside the budget.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;

use sip_message::{Method, SipMessage};
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use crate::event::TransactionEvent;

use super::call_id_budget::Admission;
use super::key::ServerTxnId;
use super::owner::Owner;

/// The way a datagram crosses the layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    In,
    Out,
}

impl Owner {
    /// Whether `message`, just received, may be dispatched: nothing under a
    /// muted Call-ID is. It counts under its Call-ID when it matches a
    /// transaction or draws an answer: every request but an ACK no server
    /// transaction holds and an out-of-dialog OPTIONS on the keepalive lane,
    /// and a response a client transaction holds. What the consumer sends
    /// for a message counted on neither side counts as it leaves.
    pub(super) fn admits_inbound(&mut self, message: &SipMessage) -> bool {
        let counted = match message {
            SipMessage::Request(req) if *req.method() == Method::Ack => {
                ServerTxnId::of_request(req).is_some_and(|id| self.server(id).is_some())
            }
            SipMessage::Request(req) => {
                !self.on_keepalive_lane(req.method(), req.to().tag().is_none())
            }
            SipMessage::Response(resp) => {
                resp.top_via().branch().is_some_and(|branch| self.clients.contains_key(branch))
            }
        };
        let call_id = message.call_id().as_str();
        if counted {
            self.within_budget(call_id, Direction::In)
        } else {
            self.not_muted(call_id, Direction::In)
        }
    }

    /// Whether a request of `method`, out of dialog when `tagless`, rides
    /// the keepalive lane: an out-of-dialog OPTIONS the layer holds no
    /// transaction for, and its answer.
    pub(super) fn on_keepalive_lane(&self, method: &Method, tagless: bool) -> bool {
        self.stateless_options && *method == Method::Options && tagless
    }

    /// Send `buf` to `dest`, counted under `call_id`; nothing leaves under a
    /// muted Call-ID.
    pub(super) async fn send_buffer(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_id: &str,
        buf: &[u8],
        dest: SocketAddr,
    ) {
        if self.within_budget(call_id, Direction::Out) {
            self.send_uncounted(endpoint, buf, dest).await;
        }
    }

    /// Send `buf` to `dest` outside every budget: the 400 refusing a parse,
    /// an answer on the keepalive lane.
    pub(super) async fn send_uncounted(
        &self,
        endpoint: &dyn UdpEndpoint,
        buf: &[u8],
        dest: SocketAddr,
    ) {
        self.metrics.count_send(endpoint.send_to(buf, dest).await);
    }

    /// Count one datagram under `call_id` going `direction`; `false` when a
    /// mute drops it, the one that trips the budget included.
    fn within_budget(&mut self, call_id: &str, direction: Direction) -> bool {
        match self.budgets.admit(call_id, Instant::now()) {
            Admission::Counted => true,
            Admission::Uncounted => {
                self.metrics.message_budget_uncounted.fetch_add(1, Relaxed);
                true
            }
            Admission::Muted => {
                self.count_muted(direction);
                false
            }
            Admission::Tripped(trip) => {
                self.metrics.message_budget_trips.fetch_add(1, Relaxed);
                self.count_muted(direction);
                tracing::warn!(
                    call_id = %trip.call_id,
                    ?direction,
                    calls = ?trip.call_refs,
                    callers = ?trip.caller_tags,
                    "a Call-ID passed its message budget: muted"
                );
                self.emit_critical(TransactionEvent::CallIdBudgetTripped(trip));
                false
            }
        }
    }

    /// Whether a datagram under `call_id` going `direction`, outside the
    /// count, passes: `false` under a mute, which it extends.
    pub(super) fn not_muted(&mut self, call_id: &str, direction: Direction) -> bool {
        if self.budgets.drops(call_id, Instant::now()) {
            self.count_muted(direction);
            return false;
        }
        true
    }

    fn count_muted(&self, direction: Direction) {
        let counter = match direction {
            Direction::In => &self.metrics.muted_in,
            Direction::Out => &self.metrics.muted_out,
        };
        counter.fetch_add(1, Relaxed);
    }
}
