//! The §16 routing ladder for one inbound request: preflight checks, the
//! non-2xx ACK hop decision, Route preprocessing + direction, self-gate
//! admission, target selection, Via push, LRU memos, freeze + forward. The
//! pushed branch is `crate::branch`'s. Route preprocessing lives in
//! [`classify`](super::classify), the gate in [`admission`](super::admission),
//! the target's steps in [`target`](super::target), Record-Route insertion in
//! [`record_route`](super::record_route), self-generated finals in
//! [`reply`](super::reply).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::emergency::is_emergency_request;
use sip_message::header::{ProxyRequire, Unsupported, Via};
use sip_message::{hops, Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::branch::stateless_branch;
use crate::cancel_lru::{invite_txn_key, retransmit_key, CancelEntry, Lifetime};
use crate::headers::{via_marking_same_face, via_with_cookie};
use crate::observability::metrics::{
    Direction, Face, MessageResult, NoTargetDrop, RoutingDecisionKind,
};
use crate::resolver::OnSent;
use crate::self_gate::BypassKind;
use crate::strategy::SelectOpts;
use sip_message::received::observed_source;

use super::super::ProxyCore;
use super::reply::extra_header;
use super::target::{repeat_hop, Chosen};
use super::{received_branch, Arrival, RouteOutcome};

impl ProxyCore {
    /// What a send to `target` tells just before its packet is handed to the
    /// socket, when `target` is a name: the memo entries under `keys`, written
    /// for this forward, settle on the address the packet goes to
    /// ([`CancelBranchLru::settle_name`]). `None` for an
    /// IP literal, which no lookup can move.
    ///
    /// [`CancelBranchLru::settle_name`]: crate::cancel_lru::CancelBranchLru::settle_name
    fn settle_on_send(&self, target: &ProxyAddr, keys: Vec<String>) -> Option<OnSent> {
        if keys.is_empty() || target.to_socket_addr().is_some() {
            return None;
        }
        let (memo, name) = (self.cancel_lru.clone(), target.clone());
        Some(Arc::new(move |sent_to| {
            for key in &keys {
                memo.settle_name(key, &name, sent_to);
            }
        }))
    }

    /// Drops a request whose own edit the draft refuses, counted as a reject:
    /// half an edit on the wire is a request with no path back for its
    /// response.
    fn drop_unforwardable(&self) -> RouteOutcome {
        self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
        self.metrics.record_reject("drop_unforwardable");
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    /// Routes one request down the RFC 3261 §16 ladder: forwards it, answers it
    /// or drops it.
    ///
    /// - An ACK is never answered (§17): an ACK past its hops, with an
    ///   unsupported or unreadable Proxy-Require, or acknowledging a final this
    ///   hop sent, ends here.
    /// - A retransmission, matched on the whole received transaction so a
    ///   request that only shares the branch is none, repeats its first copy's
    ///   target (toward a name, the address that copy left for, on the name's
    ///   face) and both
    ///   cookies from the memo. It is not counted as a new call
    ///   and meets no gate. A first target no longer `Alive` is replaced by
    ///   where the first copy's cookie places the call. A CANCEL never takes
    ///   this path; it follows its INVITE's entry.
    /// - A copy of a refused request draws the refusal again and is not judged
    ///   again (§8.2.7).
    /// - An INVITE's entry keeps the cookie a CANCEL re-resolves a dead worker
    ///   through: the verified one its Route carried, else the one its
    ///   Record-Route was minted with. A worker-outbound INVITE keeps none.
    pub(in crate::core) async fn route_request(
        &self,
        msg: &SipMessage,
        src: SocketAddr,
        face: Face,
    ) -> RouteOutcome {
        let arrival = Arrival { src, face };
        let SipMessage::Request(req) = msg else {
            self.metrics.record_reject("non_request");
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        };
        let Some(received_branch) = received_branch(req) else {
            tracing::error!("DEFECT: a request past ingress has no RFC 3261 branch");
            return self.drop_unforwardable();
        };
        let method = req.method().clone();
        let call_id = req.call_id();
        let from = req.from();
        let cseq = req.cseq();
        let select = RoutingDecisionKind::SelectNew;

        if hops::hops_exhausted(req) {
            if method == Method::Ack {
                self.metrics.record_reject("ack_max_forwards_exhausted");
                return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
            }
            return self
                .reply_reject(req, arrival, 483, "Too Many Hops", &[], "too_many_hops")
                .await;
        }
        let mf_next = hops::forwarded_max_forwards(req);

        if self.is_own_source(src) {
            return self
                .reply_reject(req, arrival, 482, "Loop Detected", &[], "loop_detected")
                .await;
        }

        match req.token_set::<ProxyRequire>() {
            Some(Ok(required)) if !required.is_empty() => {
                if method == Method::Ack {
                    self.metrics.record_reject("ack_proxy_require_unsupported");
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
                let extra = [extra_header(Unsupported::of(required.iter()))];
                return self
                    .reply_reject(
                        req,
                        arrival,
                        420,
                        "Bad Extension",
                        &extra,
                        "proxy_require_unsupported",
                    )
                    .await;
            }
            Some(Err(e)) => {
                tracing::error!(
                    reason = %e.reason,
                    "DEFECT: a Proxy-Require past ingress does not read"
                );
                if method == Method::Ack {
                    self.metrics.record_reject("ack_proxy_require_unreadable");
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
                return self
                    .reply_reject(req, arrival, 400, "Bad Request", &[], "proxy_require_unreadable")
                    .await;
            }
            _ => {}
        }

        let mut ack_hop: Option<CancelEntry> = None;
        if method == Method::Ack {
            if self.refusals.acknowledges_own_final(req)
                || self.parse_refusals.acknowledges_own_final(req)
                || self.refusals.acknowledges_refusal(req, self.now_ms())
            {
                return RouteOutcome { decision: select, target: None };
            }
            let key = crate::cancel_lru::ack_hop_key(
                req.top_via(),
                call_id.as_str(),
                from.tag(),
                cseq.seq(),
            );
            ack_hop = self.cancel_lru.lookup(&key);
        }

        let rtx_key = retransmit_key(
            req.top_via(),
            call_id.as_str(),
            from.tag(),
            method.as_str(),
            cseq.seq(),
        );
        let rtx_hit: Option<CancelEntry> =
            if method == Method::Cancel { None } else { self.cancel_lru.lookup(&rtx_key) };
        let cancelled_invite: Option<CancelEntry> = if method == Method::Cancel {
            let key = invite_txn_key(req.top_via(), call_id.as_str(), from.tag(), cseq.seq());
            self.cancel_lru.lookup(&key)
        } else {
            None
        };

        let Some((mut draft, c)) = self.classify(msg, req, src, face, req.thaw()) else {
            return self.drop_unforwardable();
        };

        if method != Method::Ack && method != Method::Cancel {
            if let Some(repeated) = self.repeat_refusal(req, arrival).await {
                return repeated;
            }
        }

        let has_to_tag = req.to().tag().is_some();
        if method == Method::Invite && !has_to_tag && rtx_hit.is_none() {
            self.metrics.record_call();
            // ADR-0026 §4
            self.activate_trace(req, src, self.trace_ms());
        }

        let is_emergency = is_emergency_request(req);
        if rtx_hit.is_none() && method == Method::Invite && !has_to_tag {
            if c.worker_outbound && !is_emergency {
                self.self_gate.note_bypass(BypassKind::Internal);
            } else if let Err(refused) = self.admit_new_call(req, arrival, is_emergency).await {
                return refused;
            }
        }

        // ADR-0009 §X7
        let looks_at_invite = if c.worker_outbound {
            ack_hop.is_some()
        } else {
            ack_hop.is_none() && c.loose_route_next_hop.is_none()
        };
        let placed_ack =
            if method == Method::Ack && looks_at_invite { self.invite_hop(msg, req) } else { None };
        let chosen: Result<Chosen, RouteOutcome> = if method == Method::Cancel {
            let target = self.cancel_target(msg, req, arrival, &c, cancelled_invite.as_ref()).await;
            target.map(|t| (t, RoutingDecisionKind::Cancel, None))
        } else if let Some(found) = ack_hop.filter(|_| !c.worker_outbound) {
            Ok((found.target, RoutingDecisionKind::AckHop, None))
        } else if let Some(found) = placed_ack {
            found.map(|t| (t, RoutingDecisionKind::AckHop, None))
        } else if let Some(next) = c.loose_route_next_hop.clone() {
            Ok((repeat_hop(next, rtx_hit.as_ref()), RoutingDecisionKind::LooseRoute, None))
        } else if c.worker_outbound {
            let target = self.request_uri_target(req, arrival, &c).await;
            let target = target.map(|t| repeat_hop(t, rtx_hit.as_ref()));
            target.map(|t| (t, RoutingDecisionKind::WorkerOutbound, None))
        } else if let Some(cookie) = &c.cookie {
            self.cookie_target(msg, req, arrival, cookie, rtx_hit.as_ref()).await
        } else if let Some(found) = &rtx_hit {
            let went = found.target.clone();
            match self.follow_live_hop(went, found.minted.as_ref(), msg) {
                Some(target) => Ok((target, RoutingDecisionKind::SelectNew, None)),
                None => Err(self.drop_without_target(NoTargetDrop::Initial)),
            }
        } else if method == Method::Ack {
            let target = self.admitted_target(msg, None, NoTargetDrop::Initial);
            target.map(|t| (t, RoutingDecisionKind::AckHop, None))
        } else {
            let admit_as_new = self.admits_as_new(msg, face, None);
            let opts = SelectOpts { admit_as_new, ..SelectOpts::default() };
            let target = self.select_target(msg, req, arrival, opts).await;
            target.map(|t| (t, RoutingDecisionKind::SelectNew, None))
        };
        let (target, decision, stickiness) = match chosen {
            Ok(chosen) => chosen,
            Err(refused) => return refused,
        };

        let memo = rtx_hit.as_ref().or(cancelled_invite.as_ref());
        let settled_from_a_name = memo.is_some_and(|found| found.settled_on(&target));
        let egress =
            self.egress.request(face, c.worker_outbound, c.loose_route_next_hop.is_some(), || {
                c.reentry
                    || (!settled_from_a_name && self.registry.lookup_by_address(&target).is_some())
            });

        let (src_host, src_port) = observed_source(src);
        draft = match draft.update_top::<Via>(|via| via.stamped_from(&src_host, src_port)) {
            Ok(d) => d,
            Err(_) => return self.drop_unforwardable(),
        };
        draft = draft.set(mf_next);

        let repeated = rtx_hit.as_ref().and_then(|found| found.minted.clone());
        let (routed, minted_cookie) = self.insert_double_record_route(
            draft,
            msg,
            req,
            src,
            &target,
            c.worker_outbound,
            &c.via_worker,
            repeated.as_ref(),
            (face, egress),
        );
        draft = routed;
        let originator = c.via_worker.clone().unwrap_or_else(|| ProxyAddr::from(src));
        let via_cookie = match rtx_hit.as_ref() {
            Some(found) => found.via_cookie.clone(),
            None => self.via_cookie(msg, req, &c, &originator, minted_cookie.as_ref()),
        };

        let our_branch = stateless_branch(req, received_branch);
        let egress_adv = self.advertised_of(egress);
        let pushed = Via::udp(egress_adv.host.as_str(), egress_adv.port)
            .with_branch(our_branch.as_str())
            .requesting_rport();
        let pushed = if self.egress.marks_the_hop(face, egress) {
            via_marking_same_face(pushed)
        } else {
            pushed
        };
        draft = draft.push_front(via_with_cookie(pushed, via_cookie.as_ref()));

        // ADR-0050 §4
        let went = rtx_hit.as_ref().map_or_else(|| target.clone(), |found| found.target.clone());
        let mut memo_keys = Vec::with_capacity(2);
        if method != Method::Cancel {
            self.cancel_lru.remember(
                &rtx_key,
                CancelEntry {
                    target: went.clone(),
                    branch: our_branch.clone(),
                    stickiness: None,
                    minted: minted_cookie.clone(),
                    via_cookie: via_cookie.clone(),
                    settled: false,
                },
                Lifetime::Retransmission,
            );
            memo_keys.push(rtx_key);
        }

        if method == Method::Invite {
            let key = invite_txn_key(req.top_via(), call_id.as_str(), from.tag(), cseq.seq());
            let carried = c.cookie.as_ref().and_then(|p| self.strategy.verified_cookie(p, msg));
            let stickiness =
                if c.worker_outbound { None } else { carried.or(minted_cookie.clone()) };
            let entry = CancelEntry {
                target: went,
                branch: our_branch,
                stickiness,
                minted: None,
                via_cookie: via_cookie.clone(),
                settled: false,
            };
            self.cancel_lru.remember(&key, entry, Lifetime::InviteTransaction);
            self.cancel_lru.publish(&self.metrics);
            memo_keys.push(key);
        }

        let Ok(bytes) = draft.freeze_bytes() else { return self.drop_unforwardable() };
        let on_sent = self.settle_on_send(&target, memo_keys);
        let refused = self.send_to(&bytes, &target, egress, on_sent).await;
        if refused == Some(sip_net::SendErrorKind::MessageTooLong) {
            return self.reply_too_large(req, arrival).await;
        }
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);
        self.trace_forward(call_id.as_str(), decision, &target, egress, stickiness, &bytes);

        RouteOutcome { decision, target: Some(target) }
    }
}
