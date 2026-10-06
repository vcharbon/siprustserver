//! [`Turn`] — one event's turn on its call: the item the router offers to
//! the call's queue, handed back whole if the queue discards it, and run as
//! the call's handler body ([`process`]) when its worker takes it. A body
//! aborted before its first poll still pays the forget its request's row
//! owes a discard ([`UnpolledForget`]).

use std::sync::Arc;

use sip_message::SipMessage;
use sip_txn::TransactionLayer;

use super::admit::UnbornCall;
use super::owed::TxnKey;
use super::process::process;
use super::resolve::Resolution;
use super::RouterCtx;
use crate::dispatch::{DispatchBody, DispatchClass, Owed, Runnable};
use b2bua_sdk::event::CallEvent;

pub(crate) struct Turn {
    pub(super) ctx: Arc<RouterCtx>,
    pub(super) event: CallEvent,
    pub(super) res: Resolution,
    pub(super) class: DispatchClass,
    /// A new call admitted at ingress, counted until its turn creates it.
    pub(super) unborn: Option<UnbornCall>,
}

impl Runnable for Turn {
    fn into_body(self) -> DispatchBody {
        let unpolled = UnpolledForget::arm(&self.ctx.txn, &self.event, self.class);
        Box::pin(async move {
            unpolled.disarm();
            process(&self.ctx, self.event, self.res, self.unborn).await;
        })
    }
}

/// Rides a handler body from the worker to its first poll. Dropped armed —
/// the body aborted before it ran — it pays the forget the request's row owes
/// a discard ([`Owed::Forget`]). Disarmed once the body runs: from then on
/// the body owns the answer.
struct UnpolledForget {
    txn: TransactionLayer,
    /// `None` once disarmed, or for an event owed no forget.
    key: Option<TxnKey>,
}

impl UnpolledForget {
    /// Armed for a request of `class` whose row owes a forget when it never
    /// runs.
    fn arm(txn: &TransactionLayer, event: &CallEvent, class: DispatchClass) -> Self {
        let key = match event {
            CallEvent::Sip { message, .. } if class.row().unrun == Owed::Forget => {
                match message.as_ref() {
                    SipMessage::Request(req) => TxnKey::of(req),
                    SipMessage::Response(_) => None,
                }
            }
            _ => None,
        };
        Self { txn: txn.clone(), key }
    }

    /// The body is running: it answers the request, or deliberately does not.
    fn disarm(mut self) {
        self.key = None;
    }
}

impl Drop for UnpolledForget {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            key.forget(&self.txn);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;
    use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

    use super::*;
    use crate::router::resolve::resolve;
    use crate::router::test_support::{node_on, Node, NODE_SIP_ADDR};

    const PEER: &str = "10.0.0.1:5060";

    /// A peer's BYE naming call `c` in its Request-URI.
    fn bye(branch: &str) -> Vec<u8> {
        format!(
            "BYE sip:b2bua@127.0.0.2:5080;callRef=c SIP/2.0\r\n\
             Via: SIP/2.0/UDP {PEER};branch={branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.2>;tag=b\r\n\
             Call-ID: turn@unit\r\n\
             CSeq: 2 BYE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A node holding call `c`'s lock, and the `Turn` of a peer BYE whose
    /// server transaction is still `Trying`: the router's own turn of it
    /// waits on the lock the test holds.
    async fn held_bye(
        branch: &str,
    ) -> (Node, Box<dyn UdpEndpoint>, tokio::sync::OwnedMutexGuard<()>, Turn) {
        let net = SimulatedSignalingNetwork::new(1);
        let node = node_on(&net, "w0", |_| {}, Vec::new()).await;
        let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
        let ctx = node.core.router_ctx().clone();
        let held = ctx.state.lock("c").await;
        let raw = bye(branch);
        peer.send_to(&raw, SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let event = CallEvent::Sip {
            message: Box::new(CustomParser::new().parse(&raw).unwrap()),
            src: PEER.parse().unwrap(),
            matched_client_txn: false,
        };
        let class = DispatchClass::of(&event).expect("a classed event");
        let res = resolve(&ctx, &event);
        assert_eq!(res.call_ref.as_deref(), Some("c"));
        (node, peer, held, Turn { ctx, event, res, class, unborn: None })
    }

    /// A production turn of a BYE aborted before its first poll never ran:
    /// its transaction is forgotten, so the peer's retransmission is
    /// admitted afresh.
    #[tokio::test(start_paused = true)]
    async fn a_turn_dropped_before_its_first_poll_forgets_its_request() {
        let (node, _peer, held, turn) = held_bye("z9hG4bK-unpolled").await;
        let txn = node.core.router_ctx().txn.clone();
        drop(turn.into_body());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(txn.metrics().unanswered_forgotten(), 1);
        drop(held);
    }

    /// A production turn of a BYE that ran, then was aborted, owns its
    /// answer: nothing is forgotten.
    #[tokio::test(start_paused = true)]
    async fn a_turn_dropped_after_its_first_poll_forgets_nothing() {
        let (node, _peer, held, turn) = held_bye("z9hG4bK-polled").await;
        let txn = node.core.router_ctx().txn.clone();
        let mut body = turn.into_body();
        // Its first poll disarms the forget, then parks on the held lock.
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(body.as_mut().poll(&mut cx).is_pending());
        drop(body);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(txn.metrics().unanswered_forgotten(), 0);
        drop(held);
    }

    /// A turn carrying an admitted new call's hold releases it whether the
    /// body is dropped before its first poll or after: an aborted or
    /// discarded turn leaves no call counted unborn.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_turn_releases_its_unborn_call() {
        for polled in [false, true] {
            let (node, _peer, held, mut turn) = held_bye("z9hG4bK-unborn").await;
            let ctx = node.core.router_ctx().clone();
            turn.unborn = Some(ctx.unborn.admit("c", 1));
            assert_eq!(ctx.unborn.count(), 1);
            let mut body = turn.into_body();
            if polled {
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(body.as_mut().poll(&mut cx).is_pending());
            }
            drop(body);
            assert_eq!(ctx.unborn.count(), 0, "polled={polled}");
            drop(held);
        }
    }

    /// A peer's new INVITE on call `copy@unit` / `a`.
    fn new_invite() -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.2:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP {PEER};branch=z9hG4bK-copy\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.2>\r\n\
             Call-ID: copy@unit\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.1:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A new INVITE's turn carrying no admission hold — a copy offered
    /// unjudged — that finds no call creates none: it is refused and counted
    /// as a copy, so no call is born past the admission ladder.
    #[tokio::test(start_paused = true)]
    async fn a_copy_turn_that_finds_no_call_creates_none() {
        let net = SimulatedSignalingNetwork::new(1);
        // The router's own judgement of the INVITE refuses it on the empty
        // bucket, opening no queue: the turn below is the only one.
        let node = node_on(
            &net,
            "w0",
            |c| {
                c.cps_bucket_size = 0;
                c.cps_bucket_rate = 0;
                c.overload_panic_elu_threshold = 1.1;
            },
            Vec::new(),
        )
        .await;
        let peer = net.bind_udp(BindUdpOpts::new(PEER.parse().unwrap(), 64)).await.unwrap();
        let ctx = node.core.router_ctx().clone();
        let raw = new_invite();
        peer.send_to(&raw, SocketAddr::from(NODE_SIP_ADDR)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let event = CallEvent::Sip {
            message: Box::new(CustomParser::new().parse(&raw).unwrap()),
            src: PEER.parse().unwrap(),
            matched_client_txn: false,
        };
        let class = DispatchClass::of(&event).expect("a classed event");
        let res = resolve(&ctx, &event);
        let turn = Turn { ctx: ctx.clone(), event, res, class, unborn: None };
        turn.into_body().await;
        assert_eq!(ctx.state.active_count(), 0, "no call is born");
        let counts = crate::new_calls::NewCallCounts::compose(
            ctx.metrics.new_calls(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(counts.total(), 1, "the router's own refusal alone");
        assert_eq!(counts.refused_copies(), 1);
    }
}
