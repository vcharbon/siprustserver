//! The §16 routing ladder for one inbound request: preflight checks, the
//! non-2xx ACK hop decision, Route preprocessing + worker-outbound
//! classification, self-gate admission, target selection, Via push, LRU memos,
//! freeze + forward. The pushed branch is `crate::branch`'s.
//! Record-Route insertion lives in [`record_route`](super::record_route);
//! self-generated finals in [`reply`](super::reply).

use std::net::SocketAddr;

use sip_message::emergency::is_emergency_request;
use sip_message::header::{MaxForwards, ProxyRequire, RouteEntry, Unsupported, Uri, Via};
use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::branch::stateless_branch;
use crate::cancel_lru::{invite_txn_key, retransmit_key, CancelEntry};
use crate::headers::{cookie_params, route_target};
use crate::observability::metrics::{CancelLookup, Direction, MessageResult, RoutingDecisionKind};
use crate::self_gate::{AdmitDecision, BypassKind};
use crate::strategy::{DecodeResult, SelectOpts};
use crate::trace::emit;

use super::super::{is_dialog_creating, ProxyCore};
use super::reply::{extra_header, proxy_reason, reject_retry_after};
use super::{top_via_branch, RouteOutcome};

/// The hop budget RFC 3261 §8.1.1.6 gives a request that names none — and the
/// one a proxy applies to a value no reader can make sense of, so a malformed
/// count neither exhausts the loop bound nor lets a request ride forever.
const DEFAULT_MAX_FORWARDS: u32 = 70;

impl ProxyCore {
    /// The top-Via sent-by when it is one of our registered workers — i.e. the
    /// request was originated *by* that worker (e.g. a B2BUA in-dialog
    /// keepalive OPTIONS toward the far endpoint, or a b-leg INVITE).
    /// SNAT-immune: the worker's advertised identity rides the message, unlike
    /// the UDP source which the VIP masquerades. Used as the worker-outbound
    /// discriminator AND as the originator identity the stickiness cookie is
    /// encoded for.
    fn worker_via_sent_by(&self, req: &SipRequest) -> Option<ProxyAddr> {
        let top = req.top_via();
        let (host, port) = top.sent_by().pair();
        let addr = ProxyAddr::new(host, port);
        self.registry.lookup_by_address(&addr).is_some().then_some(addr)
    }

    /// Whether a Route / Record-Route URI names THIS proxy. Dual-face: a self
    /// entry may carry EITHER face's advertise (the §16.6 double-RR stamps the
    /// two entries with the face facing each party).
    fn is_self_route(&self, uri: &Uri) -> bool {
        let (host, port) = uri.host_port();
        self.is_self_addr(host, port)
    }

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
            self.reply(req, src, 483, "Too Many Hops", &[]).await;
            self.metrics.record_reject("too_many_hops");
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
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
                self.reply(req, src, 420, "Bad Extension", &extra).await;
                self.metrics.record_reject("proxy_require_unsupported");
                return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
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

        // ── §16.4 Route preprocessing ───────────────────────────────────────
        let mut draft = req.thaw();
        let mut stripped_route_params: Option<crate::strategy::RouteParams> = None;
        let mut is_worker_outbound = false;
        // §16.12 + double-record-route: pop ALL leading Route values that are
        // ours. The direction is read from one of them: in a run of whole
        // pairs, the first entry of the last pair; in an odd run, the first
        // entry. `;outbound` (the worker-facing half) → forward to the R-URI;
        // the stickiness cookie (the external-facing half) → decode to the
        // worker. The rest are popped and ignored. Why the last pair: ADR-0009
        // X1.
        //
        // A route set the strict reader rejects is one this hop cannot act on:
        // it rides through exactly as it arrived (no pop, no loose-route next
        // hop) rather than failing a relay over a route set that is not ours to
        // understand.
        let routes: Vec<RouteEntry> = req.list::<RouteEntry>().unwrap_or_default();
        let self_routes = routes.iter().take_while(|r| self.is_self_route(r.uri())).count();
        let direction_route = match self_routes {
            0 => None,
            n if n % 2 == 0 => routes.get(n - 2),
            _ => routes.first(),
        };
        if let Some(direction_route) = direction_route {
            let params = cookie_params(direction_route.uri());
            if params.contains_key("outbound") {
                is_worker_outbound = true;
            } else {
                stripped_route_params = Some(params);
            }
        }
        for _ in 0..self_routes {
            // §7.3.1: a UA may fold its whole route set into one comma-combined
            // header, so the pop drops one ENTRY — removing the line would take
            // a downstream proxy's Route with it and the request would bypass
            // that route set entirely.
            draft = match draft.pop_top::<RouteEntry>() {
                Ok(d) => d,
                Err(_) => return self.drop_unforwardable(),
            };
        }
        // Worker-outbound override — break the in-dialog loop. A worker-originated
        // in-dialog request (e.g. the B2BUA's A-leg keepalive OPTIONS toward the
        // UAC) carries our own Record-Route cookie but no `;outbound` param, so the
        // checks above leave it classified as `decode_forward`; left there, the
        // cookie's `w_pri` decode bounces the request straight back to a worker and
        // the real downstream endpoint (the UAC) never sees it — its keepalive
        // times out and the dialog is torn down (the steady-state long-call-loss
        // class).
        //
        // Detect the worker origin from a SNAT-immune signal. The UDP source
        // `src` is NOT reliable: behind the keepalived VIP a worker→proxy packet
        // is masqueraded to the *node* IP (and often an ephemeral port), so
        // `lookup_by_address(src)` misses every time. The worker's own advertised
        // identity rides the message instead — the top Via sent-by — which the
        // registry keys exactly (this is the same Via-based lookup the response
        // path already trusts in `core/response.rs`). Keep the `src` check as a
        // fast path for the un-NAT'd (test / pod-direct) case.
        //
        // The Via identity is computed only when something below consumes it:
        // classification (when the self-RR didn't already decide) or the
        // stickiness cookie of a dialog-creating request. The in-dialog
        // `;outbound` keepalive path keeps its zero-lookup fast path.
        let via_worker_addr = if !is_worker_outbound || is_dialog_creating(method.as_str()) {
            self.worker_via_sent_by(req)
        } else {
            None
        };
        // A run of two or more whole pairs whose last pair decodes is a request
        // re-entering through a later pass: its worker Via is the earlier
        // pass's origin and does not make it worker-outbound.
        let spiral_reentry =
            self_routes > 2 && self_routes % 2 == 0 && stripped_route_params.is_some();
        if !is_worker_outbound
            && !spiral_reentry
            && (self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some()
                || via_worker_addr.is_some())
        {
            is_worker_outbound = true;
            stripped_route_params = None;
        }

        // ── Proxy-self gate (ELU/CPS admission) ─────────────────────────────
        // A retransmission bypasses the gate entirely: its first copy was
        // already admitted and forwarded, so rejecting the re-sent copy would
        // 503 a setup that is already ringing downstream.
        let is_new_dialog_invite = method == Method::Invite && !has_to_tag;
        let is_emergency = is_emergency_request(req);
        if rtx_hit.is_none() {
            if is_new_dialog_invite && !is_emergency && !is_worker_outbound {
                if let AdmitDecision::Reject { reason, retry_after_sec } =
                    self.self_gate.try_admit_external()
                {
                    let reason = reason.label();
                    let extra = [
                        extra_header(reject_retry_after(retry_after_sec)),
                        extra_header(proxy_reason(503, reason)),
                    ];
                    // The shed fact precedes the 503 it explains: `reply` is
                    // itself an emission site (the datagram it synthesizes).
                    emit::shed(&self.traces, call_id.as_str(), self.now_ms() as i64, reason);
                    self.reply(req, src, 503, "Service Unavailable", &extra).await;
                    // A closed set: the gate's `ShedReason` labels.
                    self.metrics.record_reject(reason);
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
            } else if is_new_dialog_invite && is_emergency {
                self.self_gate.note_bypass(BypassKind::Emergency);
            } else if is_new_dialog_invite && is_worker_outbound {
                self.self_gate.note_bypass(BypassKind::Internal);
            }
        }

        // ── Loose-route next hop (a downstream proxy's surviving Route) ──────
        let mut loose_route_next_hop: Option<ProxyAddr> = None;
        if method != Method::Cancel {
            if let Some(next_route) = routes.get(self_routes) {
                if next_route.uri().is_loose_route() {
                    loose_route_next_hop = Some(route_target(next_route.uri()));
                }
            }
        }

        // ── Pick the downstream target ──────────────────────────────────────
        // Every branch below assigns both (or returns early).
        let decision;
        let target: Option<ProxyAddr>;
        // What the cookie said, for the traced routing fact — set only by the
        // branch that consulted one.
        let mut stickiness: Option<&'static str> = None;

        if method == Method::Cancel {
            // §9.1: the CANCEL's Via is its INVITE's top Via, so it finds the
            // entry of its own INVITE transaction, never another pass of a
            // spiral sharing its Call-ID, From tag and CSeq.
            let key = invite_txn_key(req.top_via(), call_id.as_str(), from.tag(), cseq.seq());
            if let Some(found) = self.cancel_lru.lookup(&key) {
                // RFC 3261 §9.1 puts the CANCEL where the INVITE went; §16.11
                // puts it on the INVITE's branch, which the survivor's rebuilt
                // INVITE transaction is keyed by. Behind this proxy the workers
                // are ONE logical UAS, so a CANCEL toward a worker follows the
                // INVITE's own stickiness cookie through the ladder every
                // in-dialog request takes — the alive primary, else the backup
                // holding the call's replica (ADR-0014). A cookie the strategy
                // cannot place (no usable backup) and a downstream target (a
                // worker's own b-leg CANCEL) go where the INVITE went.
                let mut cancel_target = found.target;
                if let Some(cookie) = &found.stickiness {
                    let decoded = self.strategy.decode_stickiness(cookie, msg).await;
                    self.metrics.record_request_decode(&decoded);
                    match decoded {
                        DecodeResult::Forward { target: t, .. }
                        | DecodeResult::ForwardBackup { target: t, .. } => cancel_target = t,
                        DecodeResult::Reject { .. } | DecodeResult::Unknown { .. } => {}
                    }
                }
                target = Some(cancel_target);
                decision = RoutingDecisionKind::Cancel;
                self.metrics.record_cancel_lookup(CancelLookup::Hit);
            } else {
                self.metrics.record_cancel_lookup(CancelLookup::Miss);
                decision = RoutingDecisionKind::Cancel;
                if is_worker_outbound {
                    // §9.1: a CANCEL goes where its INVITE went. A worker's own
                    // b-leg INVITE went to the R-URI, memo or no memo — a
                    // fresh selection would hand a worker's CANCEL to a worker
                    // that never saw the INVITE.
                    match req.request_uri() {
                        uri if !uri.is_opaque() => {
                            let (host, port) = uri.host_port();
                            target = Some(ProxyAddr::new(host, port));
                        }
                        _ => {
                            self.reply(req, src, 400, "Bad Request", &[]).await;
                            self.metrics.record_reject("malformed_request_uri");
                            return RouteOutcome {
                                decision: RoutingDecisionKind::Reject,
                                target: None,
                            };
                        }
                    }
                } else {
                    match self.strategy.select_for_new_dialog(msg, SelectOpts::default()).await {
                        Ok(t) => target = Some(t),
                        Err(e) => return self.reply_select_failure(req, src, e).await,
                    }
                }
            }
        } else if let Some(found) = ack_hop {
            // The §17.1.1.3 ACK for a relayed non-2xx final: to the node the
            // final arrived from (see the hop-decision block above). It shares
            // the INVITE's Call-ID, From tag, CSeq number and received branch,
            // so §16.11 already puts it on the INVITE's outbound branch.
            target = Some(found.target);
            decision = RoutingDecisionKind::AckHop;
        } else if let Some(next) = loose_route_next_hop {
            target = Some(next);
            decision = RoutingDecisionKind::LooseRoute;
        } else if is_worker_outbound {
            // An out-of-range R-URI port is malformed (400), not truncated —
            // `sip:host:70596` must not be forwarded to port 5060, and a URI
            // that states one does not read.
            match req.request_uri() {
                uri if !uri.is_opaque() => {
                    let (host, port) = uri.host_port();
                    target = Some(ProxyAddr::new(host, port));
                    decision = RoutingDecisionKind::WorkerOutbound;
                }
                _ => {
                    self.reply(req, src, 400, "Bad Request", &[]).await;
                    self.metrics.record_reject("malformed_request_uri");
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
            }
        } else if let Some(params) = &stripped_route_params {
            let decoded = self.strategy.decode_stickiness(params, msg).await;
            self.metrics.record_request_decode(&decoded);
            match decoded {
                DecodeResult::Forward { target: t, .. } => {
                    target = Some(t);
                    decision = RoutingDecisionKind::DecodeForward;
                    stickiness = Some("hit");
                }
                DecodeResult::ForwardBackup { target: t, .. } => {
                    target = Some(t);
                    decision = RoutingDecisionKind::DecodeForwardBackup;
                    stickiness = Some("backup");
                }
                DecodeResult::Reject { status, reason } => {
                    self.reply(req, src, status, &reason, &[]).await;
                    // The decode reasons are free-form diagnostics — one
                    // static label keeps the reason set bounded.
                    self.metrics.record_reject("stickiness_decode_rejected");
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
                DecodeResult::Unknown { is_emergency } => {
                    // A retransmit must repeat the original (random) selection,
                    // not roll the dice again — see the rtx memo above.
                    if let Some(found) = &rtx_hit {
                        target = Some(found.target.clone());
                    } else {
                        let opts = SelectOpts { emergency_override: is_emergency };
                        match self.strategy.select_for_new_dialog(msg, opts).await {
                            Ok(t) => target = Some(t),
                            Err(e) => return self.reply_select_failure(req, src, e).await,
                        }
                    }
                    decision = RoutingDecisionKind::DecodeForward;
                    stickiness = Some("miss");
                }
            }
        } else if let Some(found) = &rtx_hit {
            // Retransmitted out-of-dialog request: repeat the original
            // selection. Re-running the strategy under a changed candidate set
            // (an ELU band flip, a worker join/leave) would forward the SAME
            // §16.11 branch to a DIFFERENT worker — one INVITE transaction
            // split across two B2BUAs (double call), with the CANCEL entry
            // then overwritten to point at the second one.
            target = Some(found.target.clone());
            decision = RoutingDecisionKind::SelectNew;
        } else {
            match self.strategy.select_for_new_dialog(msg, SelectOpts::default()).await {
                Ok(t) => target = Some(t),
                Err(e) => return self.reply_select_failure(req, src, e).await,
            }
            decision = RoutingDecisionKind::SelectNew;
        }

        let Some(target) = target else {
            // Defensive — every branch above either set `target` or returned.
            self.metrics.record_reject("no_target_selected");
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        };

        // ── §16.6 received/rport, Max-Forwards, Record-Route, Via ───────────
        let src_ip = src.ip().to_string();
        draft = match draft.update_top::<Via>(|via| via.stamped_from(&src_ip, src.port())) {
            Ok(d) => d,
            Err(_) => return self.drop_unforwardable(),
        };
        draft = draft.set(mf_next);

        let (routed, minted_cookie) = self.insert_double_record_route(
            draft,
            msg,
            req,
            src,
            &target,
            is_worker_outbound,
            &via_worker_addr,
        );
        draft = routed;

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
        draft = draft.push_front(
            Via::udp(egress_adv.host.as_str(), egress_adv.port)
                .with_branch(our_branch.as_str())
                .requesting_rport(),
        );

        // Remember the target so a retransmit of THIS request repeats the
        // forward. Short TTL: retransmits stop at Timer B/F.
        if method != Method::Cancel {
            if let Some(k) = &rtx_key {
                self.cancel_lru.remember(
                    k,
                    CancelEntry {
                        target: target.clone(),
                        branch: our_branch.clone(),
                        stickiness: None,
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
            // The cookie a CANCEL re-resolves a dead worker through: the one the
            // re-INVITE carried in its Route, else the one this initial INVITE's
            // Record-Route was minted with. A worker-outbound INVITE's cookie
            // names the worker, not the downstream target, so it is not kept.
            let stickiness = if is_worker_outbound {
                None
            } else {
                stripped_route_params.clone().or(minted_cookie)
            };
            self.cancel_lru.remember(
                &key,
                CancelEntry { target: target.clone(), branch: our_branch, stickiness },
                crate::cancel_lru::INVITE_ENTRY_TTL_MS,
            );
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
