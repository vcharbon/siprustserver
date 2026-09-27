//! A request whose handler body is discarded before it runs — the call's
//! queue full, the call cap reached, the call past its lifetime cap, a
//! release draining the queue — is never left unanswered. An INVITE, whose
//! 100 Trying already stopped the UAC's retransmissions, is answered at the
//! discard site. So is any request refused on a call past its lifetime cap,
//! which refuses every retransmission too. Any other non-INVITE has its
//! server transaction forgotten, so the UAC's retransmission (RFC 3261
//! §17.1.2.2) is admitted afresh and reaches the router again.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_txn::{IdGen, TransactionLayer};

use super::responses::{build_481, build_retry_later_500};
use super::RouterCtx;
use crate::dispatch::{Discard, DiscardHook};
use crate::event::CallEvent;
use crate::metrics::{B2buaMetrics, RemovalClass};

/// Rides a handler body from dispatch to its first poll. Dropped armed — the
/// body discarded unpolled — it asks the transaction layer to forget the
/// request's transaction while that transaction has sent nothing
/// ([`TransactionLayer::forget_unanswered`]). Disarmed once the body runs:
/// from then on the body owns the answer.
pub(super) struct UnansweredGuard {
    txn: TransactionLayer,
    /// The request's server transaction key — top-Via branch, Call-ID,
    /// From-tag; `None` once disarmed, or for an event with no server
    /// transaction the forget could free.
    key: Option<TxnKey>,
}

struct TxnKey {
    branch: String,
    call_id: String,
    from_tag: String,
}

impl UnansweredGuard {
    /// Armed for a non-INVITE request other than ACK and CANCEL: the one kind
    /// of event whose server transaction absorbs its retransmissions while it
    /// waits on the router. An INVITE is answered instead ([`InviteAnswer`]),
    /// ACK has no transaction, and a CANCEL reaching the router matched none.
    pub(super) fn for_event(txn: &TransactionLayer, event: &CallEvent) -> Self {
        let key = match event {
            CallEvent::Sip { message, .. } => match message.as_ref() {
                SipMessage::Request(req)
                    if !matches!(req.method(), Method::Invite | Method::Ack | Method::Cancel) =>
                {
                    req.top_via().branch().filter(|b| !b.is_empty()).map(|branch| TxnKey {
                        branch: branch.to_string(),
                        call_id: req.call_id().as_str().to_string(),
                        from_tag: req.from().tag().unwrap_or_default().to_string(),
                    })
                }
                _ => None,
            },
            _ => None,
        };
        Self { txn: txn.clone(), key }
    }

    /// The body is running: it answers the request, or deliberately does not.
    pub(super) fn disarm(mut self) {
        self.key = None;
    }
}

impl Drop for UnansweredGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.txn.forget_unanswered(&key.branch, &key.call_id, &key.from_tag);
        }
    }
}

/// Answers a request whose handler body is discarded unrun, through its
/// server transaction (which then absorbs its retransmissions and, for an
/// INVITE, the ACK).
///
/// - An INVITE in a dialog: 481 behind the release of a terminated call or
///   past the call's lifetime cap, whose teardown is ending the dialog
///   (RFC 3261 §12.2.2); else 500 with a Retry-After (§14.2): no room, a
///   takeover copy shed while the call lives on at its primary, or an orphan
///   release that looked no dialog up. Out of a dialog: the capacity 503
///   with a Retry-After, whatever the site, since the call it would start was
///   never looked at.
/// - Any other request but ACK (which draws no response) past the call's
///   lifetime cap: 481, the answer the call gives once it is gone. At any
///   other site the transaction is forgotten instead ([`UnansweredGuard`]).
pub(super) struct DiscardAnswer<'a> {
    pub(super) txn: &'a TransactionLayer,
    pub(super) id_gen: &'a Arc<IdGen>,
    pub(super) metrics: &'a B2buaMetrics,
    pub(super) retry_after_base_sec: u32,
    pub(super) retry_after_jitter_sec: u32,
}

impl<'a> DiscardAnswer<'a> {
    pub(super) fn of(ctx: &'a RouterCtx) -> Self {
        Self {
            txn: &ctx.txn,
            id_gen: &ctx.id_gen,
            metrics: &ctx.metrics,
            retry_after_base_sec: ctx.config.retry_after_base_sec,
            retry_after_jitter_sec: ctx.config.retry_after_jitter_sec,
        }
    }

    /// The discard hook for `event`: `Some` for a request other than ACK
    /// and CANCEL (a CANCEL reaching the router matched no transaction).
    pub(super) fn hook_for(&self, event: &CallEvent) -> Option<DiscardHook> {
        let CallEvent::Sip { message, src, .. } = event else { return None };
        let SipMessage::Request(req) = message.as_ref() else { return None };
        if matches!(req.method(), Method::Ack | Method::Cancel) {
            return None;
        }
        let pending = PendingRequest {
            req: req.clone(),
            src: *src,
            txn: self.txn.clone(),
            id_gen: self.id_gen.clone(),
            metrics: self.metrics.clone(),
            retry_after_base_sec: self.retry_after_base_sec,
            retry_after_jitter_sec: self.retry_after_jitter_sec,
        };
        Some(Box::new(move |why| Box::pin(pending.answer(why))))
    }
}

/// What the answer to one discarded request needs, owned by its hook.
struct PendingRequest {
    req: SipRequest,
    src: SocketAddr,
    txn: TransactionLayer,
    id_gen: Arc<IdGen>,
    metrics: B2buaMetrics,
    retry_after_base_sec: u32,
    retry_after_jitter_sec: u32,
}

impl PendingRequest {
    /// Hand the answer to the layer and count it. A transaction that already
    /// holds its final — a CANCEL's 487 got there first — keeps it, and the
    /// layer drops this one (RFC 3261 §17.2.1); it is counted all the same.
    async fn answer(self, why: Discard) {
        if self.req.method() != Method::Invite {
            if why == Discard::Capped {
                let to_tag = self.req.to().tag().is_none().then(|| self.id_gen.new_tag());
                let resp = build_481(&self.req, to_tag.as_deref());
                let _ = self.txn.send_response(resp, self.src).await;
                self.metrics.bump_capped_request_answered();
            }
            return;
        }
        let resp = self.invite_response(why);
        let _ = self.txn.send_response(resp, self.src).await;
        self.metrics.bump_invite_discard_answered(why);
        // A new INVITE discarded unrun was never admitted: its 503 ends it.
        if self.req.to().tag().is_none() {
            self.metrics.new_calls().reject(
                crate::new_calls::Refusal::DispatchDiscard,
                sip_message::emergency::is_emergency_request(&self.req),
            );
        }
    }

    fn invite_response(&self, why: Discard) -> SipResponse {
        let in_dialog = self.req.to().tag().is_some();
        // Only a terminated or capped call is known to be ending its dialog. A
        // shed takeover copy lives on at its primary, and an orphan release
        // looked nothing up: the retry meets the orphan path's lookup and its
        // 481.
        let gone = matches!(why, Discard::Released(RemovalClass::Terminated) | Discard::Capped);
        if in_dialog && gone {
            return build_481(&self.req, None);
        }
        let roll = u64::from(self.id_gen.new_sequence_number());
        let retry_after = crate::overload::jittered_retry_after(
            self.retry_after_base_sec,
            self.retry_after_jitter_sec,
            || roll,
        );
        if in_dialog {
            build_retry_later_500(&self.req, retry_after)
        } else {
            crate::capacity::build_capacity_reject_503(
                self.id_gen.new_tag(),
                &self.req,
                retry_after,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use sip_message::header::HeaderName;
    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;
    use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
    use sip_txn::{TransactionConfig, TransactionEvent};
    use tokio::sync::{mpsc, Notify};

    use super::*;
    use crate::dispatch::{Job, PerCallDispatcher};
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

    /// An INVITE from the peer: in the dialog `a`/`b` when `to_tag`, else
    /// out of any dialog.
    fn invite(branch: &str, to_tag: bool) -> Vec<u8> {
        let to_tag = if to_tag { ";tag=b" } else { "" };
        format!(
            "INVITE sip:b2bua@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=a\r\n\
             To: <sip:bob@127.0.0.1>{to_tag}\r\n\
             Call-ID: glare@unit\r\n\
             CSeq: 3 INVITE\r\n\
             Contact: <sip:alice@10.0.0.1:5060>\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The responses `rig.peer` receives within `wait`.
    async fn responses_at_peer(rig: &Rig, wait: u64) -> Vec<SipResponse> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
        while let Ok(Some(p)) = tokio::time::timeout_at(deadline, rig.peer.recv()).await {
            if let Ok(SipMessage::Response(r)) = CustomParser::new().parse(&p.raw) {
                out.push(r);
            }
        }
        out
    }

    fn statuses(responses: &[SipResponse]) -> Vec<u16> {
        responses.iter().map(SipResponse::status).collect()
    }

    /// The Retry-After seconds `resp` states.
    fn retry_after(resp: &SipResponse) -> Option<u32> {
        resp.raw_text(HeaderName::from("Retry-After"))
            .next()
            .and_then(|v| v.as_str().trim().parse().ok())
    }

    /// The hook the router would attach to `event`.
    fn answer_hook(rig: &Rig, event: &CallEvent) -> Option<crate::dispatch::DiscardHook> {
        DiscardAnswer {
            txn: &rig.txn,
            id_gen: &rig.id_gen,
            metrics: &rig.metrics,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 5,
        }
        .hook_for(event)
    }

    /// An INVITE from the peer, taken off the layer and dispatched behind the
    /// release queued on call `c`, then the release drained.
    async fn invite_behind_the_release(
        rig: &mut Rig,
        class: RemovalClass,
        branch: &str,
        to_tag: bool,
    ) {
        let gate = park_with_release_of(rig, class).await;
        rig.peer.send_to(&invite(branch, to_tag), addr(LAYER)).await.unwrap();
        let event = next_event(rig, 100).await.expect("the INVITE reaches the router");
        let hook = answer_hook(rig, &event);
        assert!(hook.is_some(), "an INVITE carries a discard answer");
        let job = Job::new(Box::pin(async move { drop(event) })).on_discard(hook);
        rig.dispatcher.dispatch("c", job).await;
        gate.notify_one();
        while rig.dispatcher.queue_count() > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(rig.metrics.release_discards_total(), 1);
    }

    struct Rig {
        txn: TransactionLayer,
        events: mpsc::Receiver<TransactionEvent>,
        peer: Box<dyn UdpEndpoint>,
        dispatcher: PerCallDispatcher,
        metrics: B2buaMetrics,
        id_gen: Arc<IdGen>,
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
        Rig { txn, events, peer, dispatcher, metrics, id_gen: Arc::new(IdGen::seeded(7)) }
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
        park_with_release_of(rig, RemovalClass::Terminated).await
    }

    /// [`park_with_release_queued`] with a release of `class`.
    async fn park_with_release_of(rig: &Rig, class: RemovalClass) -> Arc<Notify> {
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (g, st) = (gate.clone(), started.clone());
        rig.dispatcher
            .dispatch(
                "c",
                Job::new(Box::pin(async move {
                    st.notify_one();
                    g.notified().await;
                })),
            )
            .await;
        started.notified().await;
        rig.dispatcher.enqueue_poison("c", class);
        gate
    }

    /// A peer's BYE that crosses the call's release lands behind the poison
    /// and is discarded unrun: its hook answers nothing, its transaction is
    /// forgotten, and the peer's retransmission reaches the router again
    /// (where the call is gone and the orphan path answers it).
    #[tokio::test(start_paused = true)]
    async fn a_bye_queued_behind_the_release_is_readmitted_on_its_retransmission() {
        let mut rig = rig().await;
        let gate = park_with_release_queued(&rig).await;

        rig.peer.send_to(&bye("z9hG4bK-glare"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        let ran = Arc::new(AtomicBool::new(false));
        let guard = UnansweredGuard::for_event(&rig.txn, &event);
        let hook = answer_hook(&rig, &event);
        let r = ran.clone();
        rig.dispatcher
            .dispatch(
                "c",
                Job::new(Box::pin(async move {
                    guard.disarm();
                    r.store(true, Ordering::SeqCst);
                }))
                .on_discard(hook),
            )
            .await;

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
        rig.dispatcher
            .dispatch(
                "c",
                Job::new(Box::pin(async move {
                    guard.disarm();
                    d.notify_one();
                })),
            )
            .await;
        done.notified().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rig.txn.metrics().unanswered_forgotten(), 0);

        rig.peer.send_to(&bye("z9hG4bK-ran"), addr(LAYER)).await.unwrap();
        assert!(next_event(&mut rig, 100).await.is_none(), "the Trying transaction absorbs it");
    }

    /// A peer's re-INVITE that crosses the call's release lands behind the
    /// poison and is discarded unrun. Its 100 Trying stopped the peer's
    /// retransmissions, so the discard answers it: 481, the dialog being gone
    /// (RFC 3261 §12.2.2).
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_queued_behind_the_release_is_answered_481() {
        let mut rig = rig().await;
        invite_behind_the_release(&mut rig, RemovalClass::Terminated, "z9hG4bK-reinv", true).await;
        let statuses = statuses(&responses_at_peer(&rig, 100).await);
        assert_eq!(statuses, vec![100, 481]);
        assert_eq!(
            rig.metrics
                .invite_discard_answered_of_total(Discard::Released(RemovalClass::Terminated)),
            1
        );
    }

    /// Behind the shedding of a takeover copy the dialog lives on at the
    /// call's primary: the re-INVITE is refused for now, 500 + Retry-After,
    /// never 481 (which ends the dialog, RFC 3261 §14.1).
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_queued_behind_a_self_release_is_answered_500_with_retry_after() {
        let mut rig = rig().await;
        invite_behind_the_release(&mut rig, RemovalClass::SelfRelease, "z9hG4bK-shed", true).await;
        let responses = responses_at_peer(&rig, 100).await;
        let statuses = statuses(&responses);
        assert_eq!(statuses, vec![100, 500]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
    }

    /// An orphan release looked no dialog up — on an acting backup the
    /// primary may still serve it — so the re-INVITE is refused for now,
    /// 500 + Retry-After; its retry meets the orphan path's own lookup.
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_queued_behind_an_orphan_release_is_answered_500_with_retry_after() {
        let mut rig = rig().await;
        invite_behind_the_release(&mut rig, RemovalClass::Orphan, "z9hG4bK-orph", true).await;
        let responses = responses_at_peer(&rig, 100).await;
        assert_eq!(statuses(&responses), vec![100, 500]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
        let counts = crate::new_calls::NewCallCounts::compose(rig.metrics.new_calls(), [0, 0], 0);
        assert_eq!(counts.total(), 0, "a re-INVITE is no new call");
    }

    /// An out-of-dialog INVITE behind a release — a new attempt reusing the
    /// released call's Call-ID and From-tag — names no dialog to be missing:
    /// it draws the capacity 503 and a Retry-After, and may be retried.
    #[tokio::test(start_paused = true)]
    async fn an_initial_invite_queued_behind_the_release_is_answered_503_with_retry_after() {
        let mut rig = rig().await;
        invite_behind_the_release(&mut rig, RemovalClass::Terminated, "z9hG4bK-anew", false).await;
        let responses = responses_at_peer(&rig, 100).await;
        let statuses = statuses(&responses);
        assert_eq!(statuses, vec![100, 503]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
        let counts = crate::new_calls::NewCallCounts::compose(rig.metrics.new_calls(), [0, 0], 0);
        assert_eq!(counts.rejected(crate::new_calls::Refusal::DispatchDiscard, false), 1);
        assert_eq!(counts.total(), 1, "the discarded new call is counted once");
    }

    /// Once the body starts, its hook is dropped unheard: the body owns the
    /// answer.
    #[tokio::test(start_paused = true)]
    async fn an_invite_whose_body_ran_is_not_answered_by_its_hook() {
        let mut rig = rig().await;
        rig.peer.send_to(&invite("z9hG4bK-ran-inv", true), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the INVITE reaches the router");
        let hook = answer_hook(&rig, &event);
        let done = Arc::new(Notify::new());
        let d = done.clone();
        let job = Job::new(Box::pin(async move {
            drop(event);
            d.notify_one();
        }))
        .on_discard(hook);
        rig.dispatcher.dispatch("c", job).await;
        done.notified().await;
        let statuses = statuses(&responses_at_peer(&rig, 100).await);
        assert_eq!(statuses, vec![100], "only the layer's 100 Trying");
        assert_eq!(rig.metrics.invite_discard_answered_total(), 0);
    }

    /// A call past its lifetime cap refuses a BYE and every retransmission
    /// of it, so the refusal answers it: 481 through its transaction, which
    /// the body's drop then leaves alone, and which absorbs the
    /// retransmission.
    #[tokio::test(start_paused = true)]
    async fn a_bye_refused_past_the_lifetime_cap_is_answered_481_through_its_transaction() {
        let mut rig = rig().await;
        rig.dispatcher = PerCallDispatcher::new(8, 64, 1024, rig.metrics.clone())
            .with_lifetime_cap(1, Arc::new(|_: &str| {}));
        let gate = park_with_release_of(&rig, RemovalClass::Terminated).await;
        // The parked body was the one offer the cap allows; the release
        // queued behind it is the node's own and uncounted.
        rig.peer.send_to(&bye("z9hG4bK-capped"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        let guard = UnansweredGuard::for_event(&rig.txn, &event);
        let hook = answer_hook(&rig, &event);
        assert!(hook.is_some(), "a BYE carries a discard answer");
        let job = Job::new(Box::pin(async move { guard.disarm() })).on_discard(hook);
        rig.dispatcher.dispatch("c", job).await;
        assert_eq!(rig.metrics.capped_refusals_total(), 1);

        assert_eq!(statuses(&responses_at_peer(&rig, 100).await), vec![481]);
        assert_eq!(rig.metrics.capped_request_answered_total(), 1);
        assert_eq!(rig.txn.metrics().unanswered_forgotten(), 0);
        rig.peer.send_to(&bye("z9hG4bK-capped"), addr(LAYER)).await.unwrap();
        assert!(next_event(&mut rig, 100).await.is_none(), "the transaction absorbs it");
        assert_eq!(
            statuses(&responses_at_peer(&rig, 100).await),
            vec![481],
            "and re-sends its 481"
        );
        gate.notify_one();
    }

    /// An ACK draws no response, so it carries no discard answer.
    #[tokio::test(start_paused = true)]
    async fn an_ack_carries_no_discard_answer() {
        let mut rig = rig().await;
        let ack = String::from_utf8(bye("z9hG4bK-ack")).unwrap().replace("BYE", "ACK");
        rig.peer.send_to(ack.as_bytes(), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the ACK reaches the router");
        assert!(answer_hook(&rig, &event).is_none());
    }
}
