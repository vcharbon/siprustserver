//! The unanswered-request guard: a non-INVITE request whose handler body is
//! discarded before it runs — the call's queue full, the call cap reached,
//! a release draining the queue — has its server transaction forgotten, so
//! the UAC's retransmission (RFC 3261 §17.1.2.2) is admitted afresh and
//! reaches the router again instead of being absorbed unanswered.

use sip_message::{Method, SipMessage};
use sip_txn::TransactionLayer;

use crate::event::CallEvent;

/// Rides a handler body from dispatch to its first poll. Dropped armed — the
/// body discarded unpolled — it asks the transaction layer to forget the
/// request's transaction while that transaction has sent nothing
/// ([`TransactionLayer::forget_unanswered`]). Disarmed once the body runs:
/// from then on the body owns the answer.
pub(super) struct UnansweredGuard {
    txn: TransactionLayer,
    /// The request's top-Via branch; `None` once disarmed, or for an event
    /// with no server transaction the forget could free.
    branch: Option<String>,
}

impl UnansweredGuard {
    /// Armed for a non-INVITE request other than ACK and CANCEL: the one kind
    /// of event whose server transaction absorbs its retransmissions while it
    /// waits on the router. An INVITE's 100 Trying already stopped the caller
    /// retransmitting, ACK has no transaction, and a CANCEL reaching the router
    /// matched none.
    pub(super) fn for_event(txn: &TransactionLayer, event: &CallEvent) -> Self {
        let branch = match event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                SipMessage::Request(req)
                    if !matches!(req.method(), Method::Invite | Method::Ack | Method::Cancel) =>
                {
                    req.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
                }
                _ => None,
            },
            _ => None,
        };
        Self { txn: txn.clone(), branch }
    }

    /// The body is running: it answers the request, or deliberately does not.
    pub(super) fn disarm(mut self) {
        self.branch = None;
    }
}

impl Drop for UnansweredGuard {
    fn drop(&mut self) {
        if let Some(branch) = self.branch.take() {
            self.txn.forget_unanswered(&branch);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use sip_message::parser::custom::CustomParser;
    use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
    use sip_txn::{TransactionConfig, TransactionEvent};
    use tokio::sync::{mpsc, Notify};

    use super::*;
    use crate::dispatch::PerCallDispatcher;
    use crate::metrics::{B2buaMetrics, RemovalClass};

    const LAYER: &str = "127.0.0.1:5080";
    const PEER: &str = "10.0.0.1:5060";

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn bye(branch: &str) -> Vec<u8> {
        format!(
            "BYE sip:b2bua@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.1>;tag=b\r\n\
             Call-ID: glare@unit\r\n\
             CSeq: 2 BYE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    struct Rig {
        txn: TransactionLayer,
        events: mpsc::Receiver<TransactionEvent>,
        peer: Box<dyn UdpEndpoint>,
        dispatcher: PerCallDispatcher,
        metrics: B2buaMetrics,
    }

    async fn rig() -> Rig {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(addr(LAYER), 64)).await.unwrap();
        let peer = net.bind_udp(BindUdpOpts::new(addr(PEER), 64)).await.unwrap();
        let (txn, events) = TransactionLayer::spawn(
            ep,
            Arc::new(CustomParser::new()),
            TransactionConfig::default(),
        );
        let metrics = B2buaMetrics::new();
        let dispatcher = PerCallDispatcher::new(8, 64, 1024, metrics.clone());
        Rig { txn, events, peer, dispatcher, metrics }
    }

    /// The next event the layer hands up within `wait`, as the router sees it.
    async fn next_event(rig: &mut Rig, wait: u64) -> Option<CallEvent> {
        tokio::time::timeout(Duration::from_millis(wait), rig.events.recv())
            .await
            .ok()
            .flatten()
            .map(CallEvent::from_txn)
    }

    /// Park a body on call `c` until `gate` opens, with the call's release
    /// queued behind it: the worker drains whatever lands after the poison.
    async fn park_with_release_queued(rig: &Rig) -> Arc<Notify> {
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (g, st) = (gate.clone(), started.clone());
        rig.dispatcher.dispatch(
            "c",
            Box::pin(async move {
                st.notify_one();
                g.notified().await;
            }),
        );
        started.notified().await;
        rig.dispatcher.enqueue_poison("c", RemovalClass::Terminated);
        gate
    }

    /// A peer's BYE that crosses the call's release lands behind the poison
    /// and is discarded unrun: its transaction is forgotten, and the peer's
    /// retransmission reaches the router again (where the call is gone and
    /// the orphan path answers it).
    #[tokio::test(start_paused = true)]
    async fn a_bye_queued_behind_the_release_is_readmitted_on_its_retransmission() {
        let mut rig = rig().await;
        let gate = park_with_release_queued(&rig).await;

        rig.peer.send_to(&bye("z9hG4bK-glare"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        let ran = Arc::new(AtomicBool::new(false));
        let guard = UnansweredGuard::for_event(&rig.txn, &event);
        let r = ran.clone();
        rig.dispatcher.dispatch(
            "c",
            Box::pin(async move {
                guard.disarm();
                r.store(true, Ordering::SeqCst);
            }),
        );

        gate.notify_one();
        while rig.dispatcher.queue_count() > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!ran.load(Ordering::SeqCst), "the BYE's body never ran");
        assert_eq!(rig.metrics.release_discards_total(), 1);
        assert_eq!(rig.txn.metrics().unanswered_forgotten(), 1);

        rig.peer.send_to(&bye("z9hG4bK-glare"), addr(LAYER)).await.unwrap();
        assert!(
            next_event(&mut rig, 100).await.is_some(),
            "the retransmission is admitted afresh, not absorbed"
        );
    }

    /// Once its body has run, the body owns the answer: dropping the body
    /// after its first poll forgets nothing, and the transaction keeps
    /// absorbing retransmissions (RFC 3261 §17.2.2).
    #[tokio::test(start_paused = true)]
    async fn a_request_whose_body_ran_keeps_its_transaction() {
        let mut rig = rig().await;
        rig.peer.send_to(&bye("z9hG4bK-ran"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        let guard = UnansweredGuard::for_event(&rig.txn, &event);
        let done = Arc::new(Notify::new());
        let d = done.clone();
        rig.dispatcher.dispatch(
            "c",
            Box::pin(async move {
                guard.disarm();
                d.notify_one();
            }),
        );
        done.notified().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rig.txn.metrics().unanswered_forgotten(), 0);

        rig.peer.send_to(&bye("z9hG4bK-ran"), addr(LAYER)).await.unwrap();
        assert!(next_event(&mut rig, 100).await.is_none(), "the Trying transaction absorbs it");
    }
}
