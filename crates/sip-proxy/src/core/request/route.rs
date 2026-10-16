//! The §16 routing ladder for one inbound request: preflight checks, the
//! non-2xx ACK hop decision, Route preprocessing + direction, self-gate
//! admission, target selection, Via push, LRU memos, freeze + forward. The
//! pushed branch is `crate::branch`'s. Route preprocessing lives in
//! [`classify`](super::classify), the gate in [`admission`](super::admission),
//! the target's steps in [`target`](super::target), Record-Route insertion in
//! [`record_route`](super::record_route), self-generated finals in
//! [`reply`](super::reply).

use std::net::SocketAddr;

use sip_message::emergency::is_emergency_request;
use sip_message::header::{MaxForwards, ProxyRequire, Unsupported, Via};
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::branch::stateless_branch;
use crate::cancel_lru::{invite_txn_key, retransmit_key, CancelEntry};
use crate::headers::via_with_cookie;
use crate::observability::metrics::{
    Direction, Face, MessageResult, NoTargetDrop, RoutingDecisionKind,
};
use crate::self_gate::BypassKind;
use crate::strategy::SelectOpts;

use super::super::ProxyCore;
use super::reply::extra_header;
use super::target::Chosen;
use super::{top_via_branch, RouteOutcome};

/// The hop budget RFC 3261 §8.1.1.6 gives a request that names none — and the
/// one a proxy applies to a value no reader can make sense of, so a malformed
/// count neither exhausts the loop bound nor lets a request ride forever.
const DEFAULT_MAX_FORWARDS: u32 = 70;

impl ProxyCore {
    /// Abandon a forward whose own edit the message will not allow. Unreachable
    /// in practice — every value edited on the hop was read off these same
    /// bytes moments earlier — and a drop is the only honest outcome: half an
    /// edit on the wire is a request with no path back for its response.
    fn drop_unforwardable(&self) -> RouteOutcome {
        self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
        self.metrics.record_reject("drop_unforwardable");
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }

    pub(in crate::core) async fn route_request(
        &self,
        msg: &SipMessage,
        src: SocketAddr,
        face: Face,
    ) -> RouteOutcome {
        let SipMessage::Request(req) = msg else {
            self.metrics.record_reject("non_request");
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        };
        let method = req.method().clone();
        let call_id = req.call_id();
        let from = req.from();
        let cseq = req.cseq();
        let select = RoutingDecisionKind::SelectNew;

        // ── §16.3 + Max-Forwards ────────────────────────────────────────────
        let mf = match req.header::<MaxForwards>() {
            Some(Ok(mf)) => mf,
            _ => MaxForwards::new(DEFAULT_MAX_FORWARDS),
        };
        let Some(mf_next) = mf.decremented() else {
            // §16.3 check 2: an exhausted ACK is silently discarded, never
            // answered — a response to an ACK is a stray message (the ACK
            // terminates a transaction; nothing upstream awaits a reply to it).
            if method == Method::Ack {
                self.metrics.record_reject("ack_max_forwards_exhausted");
                return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
            }
            return self.reply_reject(req, src, 483, "Too Many Hops", &[], "too_many_hops").await;
        };

        // ── §16.3 check 5: Proxy-Require ────────────────────────────────────
        // This proxy supports no proxy extensions, so any option-tag in
        // Proxy-Require is unsupported: reject with 420 (Bad Extension) and an
        // Unsupported header listing the offending tags — the request MUST NOT
        // be forwarded. ACK is exempt: a proxy never answers an ACK (it
        // terminates a transaction; nothing upstream awaits a reply), so an ACK
        // with an unsupported Proxy-Require is silently dropped, never 420'd.
        if let Some(Ok(required)) = req.header::<ProxyRequire>() {
            if !required.is_empty() {
                if method == Method::Ack {
                    self.metrics.record_reject("ack_proxy_require_unsupported");
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
                let extra = [extra_header(Unsupported::of(required.iter()))];
                return self
                    .reply_reject(
                        req,
                        src,
                        420,
                        "Bad Extension",
                        &extra,
                        "proxy_require_unsupported",
                    )
                    .await;
            }
        }

        // ── Non-2xx ACK hop decision (§17.1.1.3) ────────────────────────────
        // An ACK for a NON-2xx INVITE final belongs to the INVITE transaction
        // itself — it reuses the INVITE's top-Via branch — while an ACK for a
        // 2xx is a NEW transaction (fresh branch) that takes the normal
        // routing ladder. The `ackhop|` memo + branch match is the
        // discriminator, NOT the presence of a Route header: a worker's b-leg
        // INVITE carries a preloaded outbound-proxy Route, and §17.1.1.3 makes
        // its non-2xx ACK copy that Route verbatim, so a route-presence
        // heuristic misroutes exactly that ACK.
        // Gating on the memo (rather than merely branch-matching the
        // remembered INVITE) also keeps a takeover UA's 2xx ACK safe: a
        // restarted `IdGen` can make its fresh ACK branch ALIAS the dead
        // incarnation's INVITE branch — but no non-2xx final was ever relayed
        // for a confirmed call, so no memo exists and the ACK flows.
        //
        // What a match means depends on who generated the final:
        //  • relayed final (memo names the node the final arrived from) —
        //    RELAY the ACK there, where §16.11 puts it on the INVITE's outbound
        //    branch so the downstream server transaction (§17.2.3) matches it
        //    and stops retransmitting the final (Timer G). This transaction-less
        //    proxy never synthesizes the hop ACK itself (ADR-0022 X4; see
        //    `core/response.rs`) — the upstream's ACK is the only quench,
        //    end-to-end, which is what keeps a lossy caller hop recoverable.
        //  • the proxy's OWN final (memo written by `reply()`, empty `branch`:
        //    no downstream exists) — the ACK terminates HERE: absorb it, never
        //    run the strategy over it and hand a worker a stray ACK matching
        //    no transaction it ever created.
        let mut ack_hop: Option<CancelEntry> = None;
        if method == Method::Ack {
            let key = crate::cancel_lru::ack_hop_key(
                req.top_via(),
                call_id.as_str(),
                from.tag(),
                cseq.seq(),
            );
            if let Some(found) = self.cancel_lru.lookup(&key) {
                // The key carries the ACK's branch, so a hit with one is the
                // INVITE's own transaction. Branchless legacy fallback
                // (pre-RFC-3261 upstream): no branch to tell them apart, so
                // the route-less heuristic decides — a 2xx ACK would carry
                // the dialog's Route set.
                let same_txn = top_via_branch(req).is_some();
                let legacy_routeless = !same_txn && !req.has(&sip_message::HeaderName::Route);
                if same_txn || legacy_routeless {
                    if found.branch.is_empty() {
                        return RouteOutcome { decision: select, target: None };
                    }
                    ack_hop = Some(found);
                }
            }
        }

        // ── §16.6 / §17.2.3 retransmission memo (looked up FIRST) ───────────
        // A retransmission repeats an already-forwarded request, so it must
        // repeat the original forward exactly. The outbound branch it repeats
        // is §16.11's function of the message; what this memo adds is the SAME
        // downstream target (re-running the strategy under a changed candidate
        // set would send the same branch to a DIFFERENT worker — one INVITE
        // transaction split across two B2BUAs, a doubled call). It must also
        // not be re-counted as a new call nor re-gated: a 503 to a retransmit
        // of an admitted INVITE tears down a setup the first copy already
        // started. Keyed on the whole received transaction
        // (`cancel_lru::retransmit_key`) so a genuine retransmit matches but a
        // different request that merely collides on the branch (a restarted
        // UA's reset `IdGen`, another sender) does not.
        // CANCEL is excluded — it resolves its target from the INVITE's own
        // entry below.
        let incoming_branch = top_via_branch(req);
        let rtx_key = incoming_branch.as_ref().map(|_| {
            retransmit_key(req.top_via(), call_id.as_str(), from.tag(), method.as_str(), cseq.seq())
        });
        let rtx_hit: Option<CancelEntry> = if method == Method::Cancel {
            None
        } else {
            rtx_key.as_ref().and_then(|k| self.cancel_lru.lookup(k))
        };

        // ── §16.4 Route preprocessing + direction (`classify`) ──────────────
        let Some((mut draft, c)) = self.classify(req, src, face, req.thaw()) else {
            return self.drop_unforwardable();
        };

        // ── A copy of a refused request (§8.2.7) ────────────────────────────
        // It draws the refusal's bytes and is neither counted as a new call
        // nor judged again: a gate or a cap that cleared since must not admit
        // a transaction this hop already refused. A worker's request is never
        // refused (it bypasses the gate and goes to its Request-URI), so it
        // never looks.
        if rtx_hit.is_none()
            && !c.worker_outbound
            && method != Method::Ack
            && method != Method::Cancel
        {
            if let Some(repeated) = self.repeat_refusal(req, src).await {
                return repeated;
            }
        }

        // A new call = an initial dialog-creating INVITE (no To-tag yet), first
        // transmission only.
        let to = req.to();
        let has_to_tag = to.tag().is_some_and(|t| !t.is_empty());
        if method == Method::Invite && to.tag().is_none() && rtx_hit.is_none() {
            self.metrics.record_call();
            // The proxy's own sampling decision — independent of the worker's,
            // taken once, on the call's first INVITE (ADR-0026).
            self.activate_trace(req, src, self.now_ms() as i64);
        }

        // ── Proxy-self gate (`admission`) ───────────────────────────────────
        // A retransmission bypasses the gate entirely: its first copy was
        // already admitted and forwarded, so rejecting the re-sent copy would
        // 503 a setup that is already ringing downstream.
        let is_emergency = is_emergency_request(req);
        if rtx_hit.is_none() && method == Method::Invite && !has_to_tag {
            if c.worker_outbound && !is_emergency {
                self.self_gate.note_bypass(BypassKind::Internal);
            } else if let Err(refused) = self.admit_new_call(req, src, is_emergency).await {
                return refused;
            }
        }

        // ── Pick the downstream target (`target`) ───────────────────────────
        let chosen: Result<Chosen, RouteOutcome> = if method == Method::Cancel {
            let target = self.cancel_target(msg, req, src, c.worker_outbound).await;
            target.map(|t| (t, RoutingDecisionKind::Cancel, None))
        } else if let Some(found) = ack_hop {
            // The §17.1.1.3 ACK for a relayed non-2xx final: to the node the
            // final arrived from (see the hop-decision block above). It shares
            // the INVITE's Call-ID, From tag, CSeq number and received branch,
            // so §16.11 already puts it on the INVITE's outbound branch.
            Ok((found.target, RoutingDecisionKind::AckHop, None))
        } else if let Some(next) = c.loose_route_next_hop.clone() {
            Ok((next, RoutingDecisionKind::LooseRoute, None))
        } else if c.worker_outbound {
            let target = self.request_uri_target(req, src).await;
            target.map(|t| (t, RoutingDecisionKind::WorkerOutbound, None))
        } else if let Some(cookie) = &c.cookie {
            self.cookie_target(msg, req, src, cookie, rtx_hit.as_ref(), face).await
        } else if let Some(found) = &rtx_hit {
            // Retransmitted out-of-dialog request: repeat the original
            // selection. Re-running the strategy under a changed candidate set
            // (an ELU band flip, a worker join/leave) would forward the SAME
            // §16.11 branch to a DIFFERENT worker — one INVITE transaction
            // split across two B2BUAs (double call), with the CANCEL entry
            // then overwritten to point at the second one. A first copy's
            // worker read not alive since is replaced by where the cookie it
            // minted places the call: its backup holds the replicated call.
            let went = found.target.clone();
            match self.follow_live_hop(went, found.minted.as_ref(), msg).await {
                Some(target) => Ok((target, RoutingDecisionKind::SelectNew, None)),
                None => Err(self.drop_without_target(NoTargetDrop::Initial)),
            }
        } else {
            let admit_as_new = self.admits_as_new(msg, face, None);
            let opts = SelectOpts { admit_as_new, ..SelectOpts::default() };
            let target = self.select_target(msg, req, src, opts).await;
            target.map(|t| (t, RoutingDecisionKind::SelectNew, None))
        };
        let (target, decision, stickiness) = match chosen {
            Ok(chosen) => chosen,
            Err(refused) => return refused,
        };

        // ── §16.6 received/rport, Max-Forwards, Record-Route, Via ───────────
        let src_ip = src.ip().to_string();
        draft = match draft.update_top::<Via>(|via| via.stamped_from(&src_ip, src.port())) {
            Ok(d) => d,
            Err(_) => return self.drop_unforwardable(),
        };
        draft = draft.set(mf_next);

        // A retransmission repeats the cookies its first copy carried, on the
        // Record-Route and on the Via, whatever moved in the worker set since:
        // the downstream server transaction absorbs the copy, and the route
        // set and responses it keeps are the first copy's (§12.1.1, §17.2.1),
        // so the dialog keeps one cookie. The memo holds them for the
        // retransmission window.
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
        );
        draft = routed;
        let originator = c.via_worker.clone().unwrap_or_else(|| ProxyAddr::from(src));
        let via_cookie = match rtx_hit.as_ref() {
            Some(found) => found.via_cookie.clone(),
            None => self.via_cookie(msg, req, &c, &originator, minted_cookie.as_ref()),
        };

        // ── §16.11: the pushed branch is a function of the message ──────────
        // A retransmission of this request, and its CANCEL and non-2xx ACK,
        // compute the same value, so the downstream server transaction
        // (§17.2.3) matches them to this forward at any proxy instance and
        // whatever any memo above did or did not hold.
        let our_branch = stateless_branch(req);
        // Stamp the EGRESS face's advertise on the pushed Via (§18.1.1: the
        // sent-by must be an address the next hop can return the response to —
        // in dual-face mode that is the face the request leaves on, so the
        // response arrives back on the same face).
        let egress_adv = self.egress_advertised(&target);
        let pushed = Via::udp(egress_adv.host.as_str(), egress_adv.port)
            .with_branch(our_branch.as_str())
            .requesting_rport();
        draft = draft.push_front(via_with_cookie(pushed, via_cookie.as_ref()));

        // Remember where the request's first copy went and both cookies, so a
        // retransmit of THIS request repeats the forward. A retransmission
        // forwarded elsewhere never rewrites that first target: every later
        // copy, and the CANCEL, is judged from where the transaction lives
        // (ADR-0050 §4). Short TTL: retransmits stop at Timer B/F.
        let went = rtx_hit.as_ref().map_or_else(|| target.clone(), |found| found.target.clone());
        if method != Method::Cancel {
            if let Some(k) = &rtx_key {
                self.cancel_lru.remember(
                    k,
                    CancelEntry {
                        target: went.clone(),
                        branch: our_branch.clone(),
                        stickiness: None,
                        minted: minted_cookie.clone(),
                        via_cookie: via_cookie.clone(),
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }

        if method == Method::Invite {
            // Long TTL: a CANCEL or non-2xx final can legally arrive any time
            // inside the downstream UA's INVITE window (B2BUA SetupTimeout /
            // sip-txn INVITE_INITIAL_TIMEOUT) — see cancel_lru.rs. Keyed on
            // the transaction as received, the Via the CANCEL will repeat.
            let key = invite_txn_key(req.top_via(), call_id.as_str(), from.tag(), cseq.seq());
            // The cookie a CANCEL re-resolves a dead worker through: the verified
            // one the re-INVITE carried in its Route, else the one this initial
            // INVITE's Record-Route was minted with (a plain preloaded Route to
            // the proxy carries none). A worker-outbound INVITE's cookie names
            // the worker, not the downstream target, so it is not kept.
            let carried = c.cookie.as_ref().and_then(|p| self.strategy.verified_cookie(p, msg));
            let stickiness =
                if c.worker_outbound { None } else { carried.or(minted_cookie.clone()) };
            let entry = CancelEntry {
                target: went,
                branch: our_branch,
                stickiness,
                minted: None,
                via_cookie: via_cookie.clone(),
            };
            self.cancel_lru.remember(&key, entry, crate::cancel_lru::INVITE_ENTRY_TTL_MS);
            self.metrics.set_pending_invite_lru_size(self.cancel_lru.size() as u64);
        }

        // ── Freeze + forward ────────────────────────────────────────────────
        let Ok(bytes) = draft.freeze_bytes() else { return self.drop_unforwardable() };
        self.send_to(&bytes, &target).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);
        self.trace_forward(call_id.as_str(), decision, &target, stickiness, &bytes);

        RouteOutcome { decision, target: Some(target) }
    }
}
