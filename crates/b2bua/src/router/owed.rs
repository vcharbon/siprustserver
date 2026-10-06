//! What the router owes the sender of a request its call's queue discarded
//! unrun, so no request is left unanswered: the request comes back with its
//! [`Owed`] (the dispatch row's), rendered here from the request itself
//! ([`OwedAnswer`]).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_txn::{IdGen, ServerTxnKey, TransactionLayer};

use super::admit;
use super::responses::{build_200, build_481, build_retry_later_500};
use super::RouterCtx;
use crate::admission::{class_of, Refusals, Refused};
use crate::dispatch::{Discard, Owed};
use crate::metrics::B2buaMetrics;
use crate::new_calls::Refusal;
use b2bua_sdk::event::CallEvent;

/// Renders the owed answer of a discarded request through its server
/// transaction, which then absorbs its retransmissions and, for an INVITE,
/// the ACK. A transaction that already holds its final — a CANCEL's 487 got
/// there first — keeps it, and the layer drops this one (RFC 3261 §17.2.1);
/// it is counted all the same.
pub(super) struct OwedAnswer<'a> {
    pub(super) txn: &'a TransactionLayer,
    pub(super) id_gen: &'a Arc<IdGen>,
    pub(super) metrics: &'a B2buaMetrics,
    pub(super) refusals: &'a Refusals,
    pub(super) retry_after_base_sec: u32,
    pub(super) retry_after_jitter_sec: u32,
}

impl<'a> OwedAnswer<'a> {
    pub(super) fn of(ctx: &'a RouterCtx) -> Self {
        Self {
            txn: &ctx.txn,
            id_gen: &ctx.id_gen,
            metrics: &ctx.metrics,
            refusals: &ctx.refusals,
            retry_after_base_sec: ctx.config.retry_after_base_sec,
            retry_after_jitter_sec: ctx.config.retry_after_jitter_sec,
        }
    }

    /// Pay what `event`, discarded for `why`, is owed, and count it. Only a
    /// request is ever owed something. `admitted` when it is a new INVITE the
    /// router's rungs admitted; a new INVITE that is not is a copy of a call
    /// already here, and its refusal is counted as a copy.
    /// FIXME(stated-headers): an answer to an in-call request carries none of
    /// the call's stated headers; the discard site holds no call. Stamp it
    /// where the call is resolved.
    pub(super) async fn render(&self, event: &CallEvent, why: Discard, owed: Owed, admitted: bool) {
        let CallEvent::Sip { message, src, .. } = event else { return };
        let SipMessage::Request(req) = message.as_ref() else { return };
        match owed {
            Owed::Nothing => {}
            Owed::Forget => {
                if let Some(key) = TxnKey::of(req) {
                    key.forget(self.txn);
                }
            }
            Owed::NewCallRefused => {
                // Only the router's run loop opens a queue, with no await
                // between the shed rung and the offer, so an admitted INVITE
                // never meets the global cap here; a copy, offered unjudged,
                // may, and is counted as a copy whatever the discard.
                debug_assert!(
                    !(admitted && why == Discard::AtCap),
                    "an admitted new INVITE passed the shed rung"
                );
                let refused = Refused { reason: Refusal::DispatchDiscard, not_before_sec: 0 };
                admit::answer(self.txn, self.refusals, req, *src, refused).await;
                if admitted {
                    self.metrics.new_calls().reject(refused.reason, class_of(req));
                } else {
                    self.metrics.new_calls().refuse_copy();
                }
            }
            Owed::RetryLater => {
                let resp = build_retry_later_500(req, None, self.jittered_retry_after());
                self.send(resp, *src).await;
                self.metrics.bump_invite_discard_answered(why);
            }
            Owed::DialogGone => {
                self.send(build_481(req, None), *src).await;
                self.metrics.bump_invite_discard_answered(why);
            }
            Owed::CappedRefusal => {
                self.send(self.capped_refusal(req), *src).await;
                self.metrics.bump_capped_request_answered();
            }
        }
    }

    async fn send(&self, resp: SipResponse, dst: SocketAddr) {
        let _ = self.txn.send_response(resp, dst).await;
    }

    /// A Retry-After, in seconds, jittered over the configured base.
    pub(super) fn jittered_retry_after(&self) -> u32 {
        let roll = u64::from(self.id_gen.new_sequence_number());
        load_shed::retry_after::jittered(
            self.retry_after_base_sec,
            self.retry_after_jitter_sec,
            || roll,
        )
    }

    /// A capped call's refusal of a non-INVITE request.
    fn capped_refusal(&self, req: &SipRequest) -> SipResponse {
        match req.method() {
            Method::Bye => build_200(req),
            // The layer binds the tag of the INVITE's final, if any.
            Method::Cancel => build_481(req, None),
            _ => {
                let to_tag = req.to().tag().is_none().then(|| self.id_gen.new_tag());
                build_481(req, to_tag.as_deref())
            }
        }
    }
}

/// A request's server transaction (RFC 3261 §17.2.3: top-Via branch, sent-by
/// and method) and its dialog identity (Call-ID, From-tag).
pub(super) struct TxnKey {
    server: ServerTxnKey,
    call_id: String,
    from_tag: String,
}

impl TxnKey {
    pub(super) fn of(req: &SipRequest) -> Option<Self> {
        ServerTxnKey::of(req).map(|server| TxnKey {
            server,
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
        })
    }

    /// Forget the transaction while it has sent nothing
    /// ([`TransactionLayer::forget_unanswered`]); a no-op once it has.
    pub(super) fn forget(&self, txn: &TransactionLayer) {
        txn.forget_unanswered(&self.server, &self.call_id, &self.from_tag);
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
    use crate::dispatch::{DispatchBody, DispatchClass, Outcome, PerCallDispatcher};
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

    struct Rig {
        txn: TransactionLayer,
        events: mpsc::Receiver<TransactionEvent>,
        peer: Box<dyn UdpEndpoint>,
        dispatcher: PerCallDispatcher<DispatchBody>,
        metrics: B2buaMetrics,
        id_gen: Arc<IdGen>,
        refusals: Refusals,
        /// Whether an offered new INVITE stands for one the router's rungs
        /// admitted (else a copy of a call already here).
        admitted: bool,
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
        let id_gen = Arc::new(IdGen::seeded(7));
        let refusals = Refusals::new(5, 5, 16, &id_gen);
        Rig { txn, events, peer, dispatcher, metrics, id_gen, refusals, admitted: true }
    }

    impl Rig {
        fn answer(&self) -> OwedAnswer<'_> {
            OwedAnswer {
                txn: &self.txn,
                id_gen: &self.id_gen,
                metrics: &self.metrics,
                refusals: &self.refusals,
                retry_after_base_sec: 5,
                retry_after_jitter_sec: 5,
            }
        }

        /// Offer `body` for `event` on call `c`, as the router does: a
        /// discard is paid what it owes.
        async fn offer(&self, event: &CallEvent, body: DispatchBody) {
            let class = DispatchClass::of(event).expect("a classed event");
            let offer = self.dispatcher.offer("c", body, class);
            if let Outcome::Discarded(d) = offer.outcome {
                self.answer().render(event, d.why, d.owed, self.admitted).await;
            }
        }

        async fn drained(&self) {
            while self.dispatcher.queue_count() > 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }

    /// The next event the layer hands up within `wait`, as the router sees it.
    async fn next_event(rig: &mut Rig, wait: u64) -> Option<CallEvent> {
        tokio::time::timeout(Duration::from_millis(wait), rig.events.recv())
            .await
            .ok()
            .flatten()
            .map(CallEvent::from_txn)
    }

    /// Park a body on call `c` until the returned gate opens, with the call's
    /// release of `class` queued behind it.
    async fn park_with_release_of(rig: &Rig, class: RemovalClass) -> Arc<Notify> {
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (g, st) = (gate.clone(), started.clone());
        let body: DispatchBody = Box::pin(async move {
            st.notify_one();
            g.notified().await;
        });
        let _ = rig.dispatcher.offer("c", body, DispatchClass::Internal);
        started.notified().await;
        rig.dispatcher.release("c", class);
        gate
    }

    /// A re-INVITE from the peer, taken off the layer and offered behind the
    /// release queued on call `c`, then the release drained.
    async fn reinvite_behind_the_release(rig: &mut Rig, class: RemovalClass, branch: &str) {
        let gate = park_with_release_of(rig, class).await;
        rig.peer.send_to(&invite(branch, true), addr(LAYER)).await.unwrap();
        let event = next_event(rig, 100).await.expect("the INVITE reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        gate.notify_one();
        rig.drained().await;
        assert_eq!(rig.metrics.release_discards_total(), 1);
    }

    /// A peer's BYE that crosses the call's release lands behind it and is
    /// discarded unrun: its transaction is forgotten, and the peer's
    /// retransmission reaches the router again (where the call is gone and
    /// the orphan path answers it).
    #[tokio::test(start_paused = true)]
    async fn a_bye_queued_behind_the_release_is_readmitted_on_its_retransmission() {
        let mut rig = rig().await;
        let gate = park_with_release_of(&rig, RemovalClass::Terminated).await;

        rig.peer.send_to(&bye("z9hG4bK-glare"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        rig.offer(&event, Box::pin(async move { r.store(true, Ordering::SeqCst) })).await;

        gate.notify_one();
        rig.drained().await;
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

    /// A peer's re-INVITE that crosses the call's release lands behind it
    /// and is discarded unrun. Its 100 Trying stopped the peer's
    /// retransmissions, so the discard answers it: 481, the dialog being gone
    /// (RFC 3261 §12.2.2).
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_queued_behind_the_release_is_answered_481() {
        let mut rig = rig().await;
        reinvite_behind_the_release(&mut rig, RemovalClass::Terminated, "z9hG4bK-reinv").await;
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
        reinvite_behind_the_release(&mut rig, RemovalClass::SelfRelease, "z9hG4bK-shed").await;
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
        reinvite_behind_the_release(&mut rig, RemovalClass::Orphan, "z9hG4bK-orph").await;
        let responses = responses_at_peer(&rig, 100).await;
        assert_eq!(statuses(&responses), vec![100, 500]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
        let counts = crate::new_calls::NewCallCounts::compose(
            rig.metrics.new_calls(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(counts.total(), 0, "a re-INVITE is no new call");
    }

    /// An out-of-dialog INVITE behind a release — a new attempt reusing the
    /// released call's Call-ID and From-tag, or a copy of one — is never
    /// discarded there: it owes nothing at its offer and runs once the
    /// release is taken, on the call's next queue, whose turn answers it.
    #[tokio::test(start_paused = true)]
    async fn an_initial_invite_behind_the_release_waits_for_the_next_queue() {
        for admitted in [true, false] {
            let mut rig = rig().await;
            rig.admitted = admitted;
            let gate = park_with_release_of(&rig, RemovalClass::Terminated).await;
            rig.peer.send_to(&invite("z9hG4bK-anew", false), addr(LAYER)).await.unwrap();
            let event = next_event(&mut rig, 100).await.expect("the INVITE reaches the router");
            let removals_at_run = Arc::new(std::sync::Mutex::new(None));
            let (at_run, metrics) = (removals_at_run.clone(), rig.metrics.clone());
            let body: DispatchBody = Box::pin(async move {
                *at_run.lock().unwrap() = Some(metrics.removals_total());
            });
            rig.offer(&event, body).await;
            gate.notify_one();
            let statuses = statuses(&responses_at_peer(&rig, 100).await);
            assert_eq!(statuses, vec![100], "nothing owed at the offer; admitted={admitted}");
            assert_eq!(*removals_at_run.lock().unwrap(), Some(1), "it ran after the release");
            let counts = crate::new_calls::NewCallCounts::compose(
                rig.metrics.new_calls(),
                Default::default(),
                Default::default(),
            );
            assert_eq!(counts.total() + counts.refused_copies(), 0, "its turn counts it");
            assert_eq!(rig.metrics.release_discards_total(), 0);
            rig.dispatcher.release("c", RemovalClass::Orphan);
            rig.drained().await;
        }
    }

    /// A queued INVITE owes nothing at its offer: once its body runs, the
    /// body owns the answer.
    #[tokio::test(start_paused = true)]
    async fn an_invite_whose_body_ran_is_not_answered_by_the_discard_path() {
        let mut rig = rig().await;
        rig.peer.send_to(&invite("z9hG4bK-ran-inv", true), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the INVITE reaches the router");
        let done = Arc::new(Notify::new());
        let d = done.clone();
        rig.offer(&event, Box::pin(async move { d.notify_one() })).await;
        done.notified().await;
        let statuses = statuses(&responses_at_peer(&rig, 100).await);
        assert_eq!(statuses, vec![100], "only the layer's 100 Trying");
        assert_eq!(rig.metrics.invite_discard_answered_total(), 0);
    }

    /// A call past its lifetime cap refuses a BYE and every retransmission
    /// of it, so the refusal answers it: 200 through its transaction (the
    /// dialog still exists, RFC 3261 §15.1.2), which absorbs the
    /// retransmission.
    #[tokio::test(start_paused = true)]
    async fn a_bye_refused_past_the_lifetime_cap_is_answered_200_through_its_transaction() {
        let mut rig = rig().await;
        rig.dispatcher =
            PerCallDispatcher::new(8, 64, 1024, rig.metrics.clone()).with_lifetime_cap(1);
        let gate = park_with_capped_offer(&rig).await;
        rig.peer.send_to(&bye("z9hG4bK-capped"), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the BYE reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        assert_eq!(rig.metrics.capped_refusals_total(), 1);

        assert_eq!(statuses(&responses_at_peer(&rig, 100).await), vec![200]);
        assert_eq!(rig.metrics.capped_request_answered_total(), 1);
        assert_eq!(rig.txn.metrics().unanswered_forgotten(), 0);
        rig.peer.send_to(&bye("z9hG4bK-capped"), addr(LAYER)).await.unwrap();
        assert!(next_event(&mut rig, 100).await.is_none(), "the transaction absorbs it");
        assert_eq!(
            statuses(&responses_at_peer(&rig, 100).await),
            vec![200],
            "and re-sends its 200"
        );
        gate.notify_one();
    }

    /// Park a counted body on call `c` — the one offer a lifetime cap of 1
    /// allows — until the returned gate opens.
    async fn park_with_capped_offer(rig: &Rig) -> Arc<Notify> {
        park_on(rig, "c").await
    }

    /// Park a counted body on `call_ref` until the returned gate opens.
    async fn park_on(rig: &Rig, call_ref: &str) -> Arc<Notify> {
        let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (g, st) = (gate.clone(), started.clone());
        let body: DispatchBody = Box::pin(async move {
            st.notify_one();
            g.notified().await;
        });
        let _ = rig.dispatcher.offer(call_ref, body, DispatchClass::OtherRequest);
        started.notified().await;
        gate
    }

    /// A CANCEL matching no transaction here reaches the router; refused
    /// past the call's lifetime cap, it is answered 481 statelessly
    /// (RFC 3261 §9.2), as the router answers a stray CANCEL.
    #[tokio::test(start_paused = true)]
    async fn a_stray_cancel_refused_past_the_lifetime_cap_is_answered_481() {
        let mut rig = rig().await;
        rig.dispatcher =
            PerCallDispatcher::new(8, 64, 1024, rig.metrics.clone()).with_lifetime_cap(1);
        let gate = park_with_capped_offer(&rig).await;
        let cancel = String::from_utf8(bye("z9hG4bK-stray")).unwrap().replace("BYE", "CANCEL");
        rig.peer.send_to(cancel.as_bytes(), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the CANCEL reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        assert_eq!(statuses(&responses_at_peer(&rig, 100).await), vec![481]);
        gate.notify_one();
    }

    /// A re-INVITE finding its call's queue full is refused for now, the
    /// dialog living on: 500 + Retry-After (RFC 3261 §14.2), counted.
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_at_a_full_queue_is_answered_500_with_retry_after() {
        let mut rig = rig().await;
        rig.dispatcher = PerCallDispatcher::new(8, 1, 1024, rig.metrics.clone());
        let gate = park_on(&rig, "c").await;
        let filler: DispatchBody = Box::pin(async {});
        let _ = rig.dispatcher.offer("c", filler, DispatchClass::OtherRequest);
        rig.peer.send_to(&invite("z9hG4bK-full", true), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the INVITE reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        let responses = responses_at_peer(&rig, 100).await;
        assert_eq!(statuses(&responses), vec![100, 500]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
        assert_eq!(rig.metrics.invite_discard_answered_of_total(Discard::QueueFull), 1);
        gate.notify_one();
    }

    /// A re-INVITE for a call with no queue at the global cap is refused for
    /// now: 500 + Retry-After, counted.
    #[tokio::test(start_paused = true)]
    async fn a_reinvite_at_the_global_cap_is_answered_500_with_retry_after() {
        let mut rig = rig().await;
        rig.dispatcher = PerCallDispatcher::new(8, 8, 1, rig.metrics.clone());
        let gate = park_on(&rig, "other").await;
        rig.peer.send_to(&invite("z9hG4bK-cap", true), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the INVITE reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        let responses = responses_at_peer(&rig, 100).await;
        assert_eq!(statuses(&responses), vec![100, 500]);
        assert!(retry_after(&responses[1]).is_some_and(|s| s >= 1), "{:?}", responses[1]);
        assert_eq!(rig.metrics.invite_discard_answered_of_total(Discard::AtCap), 1);
        assert_eq!(rig.metrics.cap_drops_total(), 1);
        gate.notify_one();
    }

    /// A request out of any dialog refused past the call's lifetime cap is
    /// answered 481 with a To-tag minted for it (RFC 3261 §8.2.6.2).
    #[tokio::test(start_paused = true)]
    async fn an_out_of_dialog_request_refused_past_the_lifetime_cap_is_answered_481_with_a_tag() {
        let mut rig = rig().await;
        rig.dispatcher =
            PerCallDispatcher::new(8, 64, 1024, rig.metrics.clone()).with_lifetime_cap(1);
        let gate = park_with_capped_offer(&rig).await;
        let info = String::from_utf8(bye("z9hG4bK-ood"))
            .unwrap()
            .replace("BYE", "INFO")
            .replace("<sip:bob@127.0.0.1>;tag=b", "<sip:bob@127.0.0.1>");
        rig.peer.send_to(info.as_bytes(), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the INFO reaches the router");
        rig.offer(&event, Box::pin(async {})).await;
        let responses = responses_at_peer(&rig, 100).await;
        assert_eq!(statuses(&responses), vec![481]);
        assert!(responses[0].to().tag().is_some_and(|t| !t.is_empty()), "{:?}", responses[0]);
        assert_eq!(rig.metrics.capped_request_answered_total(), 1);
        gate.notify_one();
    }

    /// An ACK draws no response: it is owed nothing wherever it is
    /// discarded.
    #[tokio::test(start_paused = true)]
    async fn an_ack_is_owed_nothing() {
        let mut rig = rig().await;
        let ack = String::from_utf8(bye("z9hG4bK-ack")).unwrap().replace("BYE", "ACK");
        rig.peer.send_to(ack.as_bytes(), addr(LAYER)).await.unwrap();
        let event = next_event(&mut rig, 100).await.expect("the ACK reaches the router");
        let row = DispatchClass::of(&event).expect("classed").row();
        assert_eq!(
            (row.unrun, row.ended, row.capped),
            (Owed::Nothing, Owed::Nothing, Owed::Nothing)
        );
    }
}
