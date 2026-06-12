//! Request path — port of `handleRequestImpl` (ProxyCore.ts L623-1133),
//! single-endpoint. Max-Forwards → 483; hop-by-hop ACK absorption; top-Route
//! strip + worker-outbound classification; (stubbed) self-gate; target
//! selection (CANCEL LRU → loose-route next hop → worker-outbound R-URI →
//! cookie decode → select); received/rport stamping; Record-Route insertion;
//! Via push; LRU remember; serialize + forward.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::{is_emergency_request, parse_sip_uri, parse_via_params};
use sip_message::types::SipHeader;
use sip_message::{serialize, serialize_request_parts, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{call_id_cseq_key, CancelEntry};
use crate::headers::{
    build_record_route_value, first_header_value, populate_received_rport_on_top_via, prepend_header, via_sent_by_addr,
    upsert_header,
};
use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::self_gate::BypassKind;
use crate::strategy::{DecodeResult, SelectError, SelectOpts};

use super::{is_dialog_creating, ProxyCore};

/// Outcome of routing a request — what to meter after the work is done.
struct RouteOutcome {
    decision: RoutingDecisionKind,
    /// The forwarded-to target. The lib meters only the decision; the routing
    /// regression tests assert on this field.
    #[allow(dead_code)]
    target: Option<ProxyAddr>,
}

/// The `branch=` token of the TOP (first) `Via` header of an inbound request —
/// the immediate upstream's client-transaction id (RFC 3261 §8.1.1.7: globally
/// unique per transaction thanks to the `z9hG4bK` magic cookie). A
/// retransmission reuses this exact token, so it is the correlator the proxy
/// keys retransmission branch-reuse on.
fn top_via_branch(req: &SipRequest) -> Option<String> {
    let top = first_header_value(&req.headers, "via")?;
    parse_via_params(top).branch.filter(|b| !b.is_empty())
}

/// Namespaced key for the retransmission branch memo (reuses the `CancelBranchLru`
/// store). A genuine retransmission repeats the *same* request: identical Call-ID,
/// upstream branch, method AND CSeq number (RFC 3261 §17.2.3 keys a server
/// transaction on branch + sent-by + method; the CSeq number pins it further).
/// All four are required because the simulated fabric's per-worker `IdGen` resets
/// on a failover restart, so a *different* request relayed by the backup can reuse
/// a branch token the crashed primary already spent — keying on the branch alone
/// would then mis-merge two distinct transactions onto one downstream branch (and
/// the on-wire CSeq audit would skip the second as a phantom retransmit). The
/// `rtx|` prefix keeps it disjoint from `call_id_cseq_key` (`{call_id}|{cseq}`).
fn retransmit_key(call_id: &str, incoming_branch: &str, method: &str, cseq: u32) -> String {
    format!("rtx|{call_id}|{incoming_branch}|{method}|{cseq}")
}

impl ProxyCore {
    /// The top-Via sent-by when it is one of our registered workers — i.e. the
    /// request was originated *by* that worker (e.g. a B2BUA in-dialog
    /// keepalive OPTIONS toward the far endpoint, or a b-leg INVITE).
    /// SNAT-immune: the worker's advertised identity rides the message, unlike
    /// the UDP source which the VIP masquerades. Used as the worker-outbound
    /// discriminator AND as the originator identity the stickiness cookie is
    /// encoded for.
    fn worker_via_sent_by(&self, req: &SipRequest) -> Option<ProxyAddr> {
        first_header_value(&req.headers, "via")
            .and_then(via_sent_by_addr)
            .filter(|a| self.registry.lookup_by_address(a).is_some())
    }

    pub(super) async fn handle_request(&self, msg: SipMessage, src: SocketAddr) {
        let start_ms = self.now_ms();
        let SipMessage::Request(req) = &msg else { return };
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        // Method::as_str() is already canonical-uppercase for known methods
        // (Method::from_wire normalized at parse time); unknown tokens match
        // no routing branch and land in the bounded `other` metric slot.
        self.metrics.record_request(req.method.as_str());
        // (`sip_proxy_calls_total` is counted inside `route_request`, where the
        // retransmission memo can exclude re-sent copies of the same INVITE.)

        let outcome = self.route_request(&msg, src).await;

        let duration = (self.now_ms().saturating_sub(start_ms)) as f64 / 1000.0;
        self.metrics.observe_routing_duration(duration);
        self.metrics.record_routing_decision(outcome.decision);
    }

    async fn route_request(&self, msg: &SipMessage, src: SocketAddr) -> RouteOutcome {
        let SipMessage::Request(req) = msg else {
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        };
        let method = req.method.as_str();
        let select = RoutingDecisionKind::SelectNew;

        // ── §16.3 + Max-Forwards ────────────────────────────────────────────
        let mf: i64 = first_header_value(&req.headers, "max-forwards")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(70);
        if mf <= 0 {
            // §16.3 check 2: an exhausted ACK is silently discarded, never
            // answered — a response to an ACK is a stray message (the ACK
            // terminates a transaction; nothing upstream awaits a reply to it).
            if method == "ACK" {
                return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
            }
            self.reply(req, src, 483, "Too Many Hops", &[]).await;
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        }
        let mf_next = mf - 1;

        // ── Hop-by-hop ACK absorption (§17.1.1.3) ───────────────────────────
        if method == "ACK" && first_header_value(&req.headers, "route").is_none() {
            let key = call_id_cseq_key(&req.call_id, req.from.tag.as_deref(), req.cseq.seq);
            if self.cancel_lru.lookup(&key).is_some() {
                return RouteOutcome { decision: select, target: None };
            }
        }

        // ── §16.6 / §17.2.3 retransmission memo (looked up FIRST) ───────────
        // A retransmission repeats an already-forwarded request, so it must
        // repeat the original forward exactly: same outbound top-Via branch
        // (else the downstream transaction layer sees a fresh transaction at
        // the same CSeq and the on-wire §12.2.2 CSeq audit rejects it) and the
        // SAME downstream target (re-running the strategy under a changed
        // candidate set would send the same branch to a DIFFERENT worker — one
        // INVITE transaction split across two B2BUAs, a doubled call). It must
        // also not be re-counted as a new call nor re-gated: a 503 to a
        // retransmit of an admitted INVITE tears down a setup the first copy
        // already started. Keyed on the full (Call-ID, upstream branch,
        // method, CSeq) so a genuine retransmit matches but a different
        // request that merely collides on the branch (a backup's reset `IdGen`
        // after failover) does not. CANCEL is excluded — it resolves target +
        // branch from the INVITE's own entry below.
        let incoming_branch = top_via_branch(req);
        let rtx_key = incoming_branch
            .as_ref()
            .map(|b| retransmit_key(&req.call_id, b, method, req.cseq.seq));
        let rtx_hit: Option<CancelEntry> = if method == "CANCEL" {
            None
        } else {
            rtx_key.as_ref().and_then(|k| self.cancel_lru.lookup(k))
        };

        // A new call = an initial dialog-creating INVITE (no To-tag yet), first
        // transmission only.
        if method == "INVITE" && req.to.tag.is_none() && rtx_hit.is_none() {
            self.metrics.record_call();
        }

        // ── §16.4 Route preprocessing ───────────────────────────────────────
        let mut headers: Vec<SipHeader> = req.headers.clone();
        let mut stripped_route_params: Option<crate::strategy::RouteParams> = None;
        let mut is_worker_outbound = false;
        // §16.12 + double-record-route: pop ALL leading Route values that are
        // ours, and read the in-dialog direction from the FIRST one — which the
        // proxy itself chose at dialog set-up. The worker-facing half carries
        // `;outbound` (→ forward to the R-URI); the external-facing half carries
        // the stickiness cookie (→ decode to the worker). Direction is therefore
        // intrinsic to the proxy's own self-issued Record-Route, not a marker the
        // worker stamps. The partner half of the pair (the other self-RR, present
        // because we double-record-route) is popped and ignored.
        let mut first_self_route = true;
        loop {
            // Inspect (and pop) only the FIRST entry of the first Route line:
            // §7.3.1 lets a UA fold its whole route set into one comma-combined
            // header, and removing the whole line would delete a downstream
            // proxy's Route along with our own entry — the request would then
            // bypass the downstream route set entirely.
            let Some(top_route) = first_header_value(&headers, "route") else { break };
            let Some(entry) = crate::headers::split_top_level_commas(top_route).into_iter().next() else { break };
            let Some(parsed) = parse_sip_uri(&entry) else { break };
            // Out-of-range port → malformed, never truncated (70596 ≢ 5060).
            if parsed.host != self.advertised.host
                || crate::headers::uri_port_u16(parsed.port) != Some(self.advertised.port)
            {
                break;
            }
            let params = sip_message::message_helpers::parse_uri_params(&entry);
            crate::headers::remove_first_header_entry(&mut headers, "route");
            if first_self_route {
                if params.contains_key("outbound") {
                    is_worker_outbound = true;
                } else {
                    stripped_route_params = Some(params);
                }
                first_self_route = false;
            }
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
        let via_worker_addr = if !is_worker_outbound || is_dialog_creating(method) {
            self.worker_via_sent_by(req)
        } else {
            None
        };
        if !is_worker_outbound
            && (self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some() || via_worker_addr.is_some())
        {
            is_worker_outbound = true;
            stripped_route_params = None;
        }

        // ── Proxy-self gate (stubbed always-admit) ──────────────────────────
        // A retransmission bypasses the gate entirely: its first copy was
        // already admitted and forwarded, so rejecting the re-sent copy would
        // 503 a setup that is already ringing downstream.
        let has_to_tag = req.to.tag.as_deref().is_some_and(|t| !t.is_empty());
        let is_new_dialog_invite = method == "INVITE" && !has_to_tag;
        let is_emergency = is_emergency_request(req);
        if rtx_hit.is_none() {
            if is_new_dialog_invite && !is_emergency && !is_worker_outbound {
                let decision = self.self_gate.try_admit_external();
                if !decision.admit {
                    let reason = decision.reason.unwrap_or_else(|| "proxy_overload_cps".to_string());
                    let extra = [
                        SipHeader { name: "Retry-After".into(), value: decision.retry_after_sec.to_string() },
                        SipHeader { name: "Reason".into(), value: format!("SIP;cause=503;text=\"{reason}\"") },
                    ];
                    self.reply(req, src, 503, "Service Unavailable", &extra).await;
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
        if method != "CANCEL" {
            if let Some(next_route) = first_header_value(&headers, "route") {
                if sip_message::generators::first_route_is_loose(next_route) {
                    loose_route_next_hop = crate::headers::route_value_to_addr(next_route);
                }
            }
        }

        // ── Pick the downstream target ──────────────────────────────────────
        // Every branch below assigns both (or returns early).
        let decision;
        let target: Option<ProxyAddr>;
        let mut reuse_branch: Option<String> = None;

        if method == "CANCEL" {
            let key = call_id_cseq_key(&req.call_id, req.from.tag.as_deref(), req.cseq.seq);
            if let Some(found) = self.cancel_lru.lookup(&key) {
                target = Some(found.target);
                reuse_branch = Some(found.branch);
                decision = RoutingDecisionKind::Cancel;
                self.metrics.record_cancel_lookup("hit");
            } else {
                self.metrics.record_cancel_lookup("miss");
                decision = RoutingDecisionKind::Cancel;
                match self.strategy.select_for_new_dialog(msg, SelectOpts::default()).await {
                    Ok(t) => target = Some(t),
                    Err(e) => return self.reply_select_failure(req, src, e).await,
                }
            }
        } else if let Some(next) = loose_route_next_hop {
            target = Some(next);
            decision = RoutingDecisionKind::LooseRoute;
        } else if is_worker_outbound {
            // An out-of-range R-URI port is malformed (400), not truncated —
            // `sip:host:70596` must not be forwarded to port 5060.
            match parse_sip_uri(&req.uri)
                .and_then(|p| Some(ProxyAddr::new(p.host, crate::headers::uri_port_u16(p.port)?)))
            {
                Some(addr) => {
                    target = Some(addr);
                    decision = RoutingDecisionKind::WorkerOutbound;
                }
                None => {
                    self.reply(req, src, 400, "Bad Request", &[]).await;
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
            }
        } else if let Some(params) = &stripped_route_params {
            match self.strategy.decode_stickiness(params, msg).await {
                DecodeResult::Forward { target: t, .. } => {
                    target = Some(t);
                    decision = RoutingDecisionKind::DecodeForward;
                }
                DecodeResult::ForwardBackup { target: t, .. } => {
                    target = Some(t);
                    decision = RoutingDecisionKind::DecodeForwardBackup;
                }
                DecodeResult::Reject { status, reason } => {
                    self.reply(req, src, status, &reason, &[]).await;
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
                }
            }
        } else if let Some(found) = &rtx_hit {
            // Retransmitted out-of-dialog request: repeat the original
            // selection. Re-running the strategy under a changed candidate set
            // (an ELU band flip, a worker join/leave) would forward the SAME
            // reused branch to a DIFFERENT worker — one INVITE transaction
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
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        };

        // ── §16.6 received/rport, Max-Forwards, Record-Route, Via ───────────
        let src_ip = src.ip().to_string();
        populate_received_rport_on_top_via(&mut headers, &src_ip, src.port());
        upsert_header(&mut headers, "Max-Forwards", &mf_next.to_string());

        // Record-Route only on the INITIAL dialog-creating request (no To-tag). A
        // mid-dialog re-INVITE / target-refresh (To-tag present) reuses the route
        // set already fixed at dialog creation (RFC 3261 §12.2), so re-inserting RR
        // is inert bloat and never alters the established route set.
        let is_initial_dialog_req = req.to.tag.as_deref().map(|t| t.is_empty()).unwrap_or(true);
        if is_dialog_creating(method) && is_initial_dialog_req {
            // Double record-route so in-dialog DIRECTION is intrinsic to the
            // proxy's own Record-Route — no worker-stamped `;outbound`. We insert
            // two RRs:
            //   • cookie RR  — used by the EXTERNAL party to reach the worker
            //     (decode → w_pri, registry-keyed, so it survives a worker pod-IP
            //     change after reboot).
            //   • outbound RR — used by the WORKER to reach the external party
            //     (classified `;outbound` → forward to the R-URI).
            // The §12.1.1 (UAS, forward) / §12.1.2 (UAC, reverse) route-set rule
            // then puts the right half on top of each party's route set on its
            // own; here we only choose which faces the *next hop*: forwarding TO a
            // worker (inbound) puts the outbound/worker-facing RR on top, forwarding
            // to the external party (worker-outbound) puts the cookie RR on top.
            // `prepend_header` pushes onto the top, so prepend the lower half first.
            // For a worker-originated request the cookie identifies the
            // ORIGINATING worker, taken from the SNAT-immune Via identity —
            // NOT the UDP source: behind the keepalived VIP the source is the
            // node IP + an ephemeral port, which matches no registry entry, so
            // encode_stickiness silently produced a param-less cookie RR and
            // the callee's later in-dialog requests decoded Unknown and were
            // re-sharded to an arbitrary worker (the b-leg variant of the
            // long-call-loss class). `src` stays as the pod-direct fallback.
            let cookie_addr = if is_worker_outbound {
                via_worker_addr.clone().unwrap_or_else(|| ProxyAddr::from(src))
            } else {
                target.clone()
            };
            let stickiness = self.strategy.encode_stickiness(&cookie_addr, msg);
            let cookie_rr = match &stickiness {
                Some(params) => build_record_route_value(&self.advertised, params.iter()),
                None => build_record_route_value(&self.advertised, std::iter::empty()),
            };
            let outbound_rr =
                format!("<sip:{}:{};outbound;lr>", self.advertised.host, self.advertised.port);
            if is_worker_outbound {
                prepend_header(&mut headers, "Record-Route", &outbound_rr);
                prepend_header(&mut headers, "Record-Route", &cookie_rr);
            } else {
                prepend_header(&mut headers, "Record-Route", &cookie_rr);
                prepend_header(&mut headers, "Record-Route", &outbound_rr);
            }
            self.metrics.record_route_inserted();
        }

        // ── §16.6 / §17.2.3 retransmission branch reuse ─────────────────────
        // A retransmission carries the SAME outbound top-Via branch the
        // original forward used (looked up at the top of this fn); CANCEL
        // already resolved its branch from the INVITE entry above. Without
        // this, a keepalive OPTIONS that retransmits before its 200 lands
        // reaches the callee as several distinct CSeq-N transactions and the
        // on-wire §12.2.2 audit (correctly) rejects the 2nd as a CSeq reuse.
        if reuse_branch.is_none() {
            if let Some(found) = &rtx_hit {
                reuse_branch = Some(found.branch.clone());
            }
        }

        let our_branch = reuse_branch.unwrap_or_else(|| self.id_gen.new_branch());
        let via_value =
            format!("SIP/2.0/UDP {}:{};branch={};rport", self.advertised.host, self.advertised.port, our_branch);
        prepend_header(&mut headers, "Via", &via_value);

        // Remember the outbound (target, branch) so a retransmit of THIS
        // request repeats the forward. Short TTL: retransmits stop at Timer B/F.
        if method != "CANCEL" {
            if let Some(k) = &rtx_key {
                self.cancel_lru.remember(
                    k,
                    CancelEntry { target: target.clone(), branch: our_branch.clone() },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }

        if method == "INVITE" {
            // Long TTL: a CANCEL or non-2xx final can legally arrive any time
            // inside the downstream UA's INVITE window (B2BUA SetupTimeout /
            // sip-txn INVITE_INITIAL_TIMEOUT) — see cancel_lru.rs.
            let key = call_id_cseq_key(&req.call_id, req.from.tag.as_deref(), req.cseq.seq);
            self.cancel_lru.remember(
                &key,
                CancelEntry { target: target.clone(), branch: our_branch },
                crate::cancel_lru::INVITE_ENTRY_TTL_MS,
            );
            self.metrics.set_pending_invite_lru_size(self.cancel_lru.size() as u64);
        }

        // ── Serialize + forward ─────────────────────────────────────────────
        let bytes = serialize_request_parts(req, &headers);
        self.send_to(&bytes, &target).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);

        RouteOutcome { decision, target: Some(target) }
    }

    /// Synthesize a UAS response to the source.
    async fn reply(&self, req: &SipRequest, src: SocketAddr, status: u16, reason: &str, extra: &[SipHeader]) {
        let opts = GenerateResponseOpts {
            to_tag: Some(self.id_gen.new_tag()),
            extra_headers: extra.to_vec(),
            ..Default::default()
        };
        let resp = generate_response(req, status, reason, &opts);
        self.reply_to_source(&serialize(&SipMessage::Response(resp)), src).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Responded);
    }

    /// Map a `select_for_new_dialog` failure to its 503 (distinct Reason/Retry-After).
    async fn reply_select_failure(&self, req: &SipRequest, src: SocketAddr, err: SelectError) -> RouteOutcome {
        let (retry_after, reason) = match err {
            SelectError::RateCapExhausted { retry_after_sec, .. } => {
                (retry_after_sec.to_string(), "SIP;cause=503;text=\"rate_cap_exhausted\"".to_string())
            }
            SelectError::NoTarget { .. } => ("5".to_string(), "SIP;cause=503;text=\"no_target_available\"".to_string()),
        };
        let extra = [
            SipHeader { name: "Retry-After".into(), value: retry_after },
            SipHeader { name: "Reason".into(), value: reason },
        ];
        self.reply(req, src, 503, "Service Unavailable", &extra).await;
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }
}


#[cfg(test)]
mod worker_outbound_tests {
    use std::sync::Arc;

    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::types::BindUdpOpts;
    use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

    use crate::addr::ProxyAddr;
    use crate::core::ProxyCoreBuilder;
    use crate::observability::metrics::RoutingDecisionKind;
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::{WorkerEntry, WorkerRegistry};
    use crate::strategies::forward_all::ForwardAllStrategy;
    use crate::{ProxyMetrics, RoutingStrategy};

    // Worker w1 lives at its POD ip:5060 (what the registry holds and the worker
    // stamps as its Via sent-by). The downstream UAC the keepalive targets.
    const W1_POD: &str = "10.244.5.8";
    const UAC: &str = "10.244.7.13";
    const PROXY_VIP: &str = "172.20.255.250";

    async fn core(reg: Arc<dyn WorkerRegistry>) -> crate::core::ProxyCore {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
        ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(Arc::new(ProxyMetrics::new()))
            .build(ep)
    }

    // A B2BUA A-leg keepalive OPTIONS toward the UAC: top Via = the originating
    // worker, our own cookie Route (`target=worker`, no `;outbound`), R-URI = the
    // UAC. This is exactly the on-wire shape captured in the endurance repro.
    fn keepalive_options() -> SipMessage {
        let raw = format!(
            "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKkeepalive;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-1@{UAC}\r\n\
CSeq: 2 OPTIONS\r\n\
Contact: <sip:b2bua@{W1_POD}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
        );
        CustomParser::default().parse(raw.as_bytes()).unwrap()
    }

    // Regression for the steady-state long-call-loss class: behind the keepalived
    // VIP a worker→proxy packet is SNAT'd to the NODE ip:ephemeral-port, so the
    // proxy's UDP source is NOT a registered worker. The worker-outbound
    // classification must therefore key off the SNAT-immune top Via sent-by, not
    // the socket source — otherwise the cookie's `target=worker` decode bounces
    // the keepalive straight back to a worker and the UAC never sees it (350s
    // recv-timeout → BYE → 481).
    #[tokio::test]
    async fn snat_masqueraded_worker_keepalive_routes_to_downstream_not_back_to_worker() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        // SNAT'd source: the kind NODE ip + an ephemeral port — NOT in the registry.
        let snat_src = "172.20.0.11:63522".parse().unwrap();
        let msg = keepalive_options();
        let outcome = core.route_request(&msg, snat_src).await;

        assert_eq!(
            outcome.decision,
            RoutingDecisionKind::WorkerOutbound,
            "a worker-originated keepalive must be worker-outbound even when SNAT hides the source"
        );
        assert_eq!(
            outcome.target,
            Some(ProxyAddr::new(UAC, 5060)),
            "the keepalive must reach the UAC (R-URI), not bounce back to a worker via the cookie"
        );
    }

    // Regression for the recv-loop head-of-line block: a worker-outbound
    // request whose R-URI is a DNS name must NOT make routing wait on the
    // resolver — resolution happens on a spawned task (see crate::resolver).
    // Under the old inline-await design a resolver that never answers would
    // have parked route_request (and with it the whole recv loop) forever;
    // here the only pending timer is the watchdog timeout, so a regression
    // fails fast instead of hanging.
    #[tokio::test(start_paused = true)]
    async fn named_target_resolution_never_blocks_routing() {
        struct PendingResolver;
        #[async_trait::async_trait]
        impl crate::resolver::HostResolver for PendingResolver {
            async fn resolve(&self, _host: &str, _port: u16) -> Option<std::net::SocketAddr> {
                std::future::pending().await
            }
        }

        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1_POD, 5060)));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .resolver(Arc::new(PendingResolver))
            .build(ep);

        let raw = format!(
            "OPTIONS sip:sipp@uas.example:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKnamed;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@uas.example:5060>;tag=uactag\r\n\
Call-ID: named-1@uas.example\r\n\
CSeq: 2 OPTIONS\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            core.route_request(&msg, format!("{W1_POD}:5060").parse().unwrap()),
        )
        .await
        .expect("route_request must not wait on DNS resolution");
        assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
        assert_eq!(outcome.target, Some(ProxyAddr::new("uas.example", 5060)));
    }

    // The un-NAT'd fast path still works: source IS the registered worker.
    #[tokio::test]
    async fn pod_direct_worker_source_is_still_worker_outbound() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        let pod_src = format!("{W1_POD}:5060").parse().unwrap();
        let msg = keepalive_options();
        let outcome = core.route_request(&msg, pod_src).await;

        assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
        assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
    }

    // A genuine EXTERNAL in-dialog request (top Via = a non-worker UAC) must NOT
    // be misclassified as worker-outbound — it follows the cookie back to its
    // worker as before.
    #[tokio::test]
    async fn external_in_dialog_request_is_not_worker_outbound() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        // Same cookie Route, but the top Via sent-by is the UAC (not a worker).
        let raw = format!(
            "BYE sip:b2bua@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKext;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
To: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
Call-ID: longcall-1@{UAC}\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
        let outcome = core.route_request(&msg, format!("{UAC}:5060").parse().unwrap()).await;

        assert_eq!(outcome.decision, RoutingDecisionKind::DecodeForward);
        assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
    }

    // A worker-stamped `;outbound` is still accepted (backward-compatible
    // defense-in-depth), but it is NO LONGER how the b2bua operates: see the
    // double-record-route tests below for the load-bearing path.
    #[tokio::test]
    async fn worker_stamped_outbound_marker_still_accepted() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        let new_pod_ip = "10.244.9.99"; // rebooted worker's new IP — NOT in registry
        let raw = format!(
            "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {new_pod_ip}:5060;branch=z9hG4bKreboot;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-2@{UAC}\r\n\
CSeq: 3 OPTIONS\r\n\
Contact: <sip:b2bua@{new_pod_ip}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr;outbound>\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
        let snat_src = "172.20.0.12:51000".parse().unwrap();
        let outcome = core.route_request(&msg, snat_src).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
        assert_eq!(outcome.target, Some(ProxyAddr::new(UAC, 5060)));
    }

    // ── Double-record-route: direction is intrinsic to the proxy's own RR ─────
    //
    // THE load-bearing case. A worker that has just rebooted onto a NEW pod IP
    // (absent from the registry) sends, behind the SNAT'd VIP, its keepalive on
    // the route set captured at dialog set-up: the proxy's OWN `;outbound`
    // Record-Route on top, the stickiness cookie below. The worker stamps NOTHING.
    // Neither the SNAT'd source nor the top Via (the new IP) is a registered
    // worker, so every registry-based discriminator MISSES — yet direction is
    // still correct because it is read from the proxy's own self-issued top RR.
    // This is what makes `;outbound`-by-the-worker no longer load-bearing.
    #[tokio::test]
    async fn rebooted_worker_keepalive_direction_from_proxy_issued_outbound_rr() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        let new_pod_ip = "10.244.9.99"; // rebooted worker's new IP — NOT in registry
        let raw = format!(
            "OPTIONS sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {new_pod_ip}:5060;branch=z9hG4bKreboot;lg=a;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: longcall-3@{UAC}\r\n\
CSeq: 4 OPTIONS\r\n\
Contact: <sip:b2bua@{new_pod_ip}:5060;leg=a>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
        let snat_src = "172.20.0.12:51000".parse().unwrap(); // node IP, not a worker
        let outcome = core.route_request(&msg, snat_src).await;

        assert_eq!(
            outcome.decision,
            RoutingDecisionKind::WorkerOutbound,
            "direction must come from the proxy's own top `;outbound` RR — no worker marker, no registry/Via match"
        );
        assert_eq!(
            outcome.target,
            Some(ProxyAddr::new(UAC, 5060)),
            "the keepalive must reach the UAC (R-URI), not bounce back via the cookie below"
        );
    }

    // The mirror direction with the double-record-route present: an EXTERNAL
    // in-dialog request from the UAC carries the cookie on top (alice's route set
    // is the reverse of the 2xx: cookie, then outbound). The proxy pops BOTH self
    // RRs but reads direction from the top (cookie) → decode to the worker. The
    // trailing `;outbound` half must NOT flip it to worker-outbound.
    #[tokio::test]
    async fn external_in_dialog_with_double_rr_decodes_to_worker() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        let raw = format!(
            "BYE sip:b2bua@{W1_POD}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKext;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
To: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
Call-ID: longcall-3@{UAC}\r\n\
CSeq: 5 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;target={W1_POD}:5060;lr>\r\n\
Route: <sip:{PROXY_VIP}:5060;outbound;lr>\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw.as_bytes()).unwrap();
        let outcome = core.route_request(&msg, format!("{UAC}:5060").parse().unwrap()).await;

        assert_eq!(
            outcome.decision,
            RoutingDecisionKind::DecodeForward,
            "cookie on top → decode to the worker; the trailing ;outbound half must not flip direction"
        );
        assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
    }
}

#[cfg(test)]
mod retransmission_tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::types::BindUdpOpts;
    use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

    use crate::addr::ProxyAddr;
    use crate::core::{ProxyCore, ProxyCoreBuilder};
    use crate::observability::metrics::RoutingDecisionKind;
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::WorkerRegistry;
    use crate::self_gate::{AdmitDecision, ProxySelfGate};
    use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};
    use crate::ProxyMetrics;

    const UAC: &str = "10.244.7.13";
    const PROXY_VIP: &str = "172.20.255.250";
    const W1: &str = "10.0.0.1";
    const W2: &str = "10.0.0.2";

    /// Strategy double: pops targets off a queue, counting selections — a
    /// changed candidate set is modeled as "the next selection differs".
    struct QueueStrategy {
        targets: Mutex<VecDeque<ProxyAddr>>,
        calls: AtomicU32,
    }

    impl QueueStrategy {
        fn of(targets: &[ProxyAddr]) -> Arc<Self> {
            Arc::new(Self { targets: Mutex::new(targets.iter().cloned().collect()), calls: AtomicU32::new(0) })
        }
    }

    #[async_trait]
    impl RoutingStrategy for QueueStrategy {
        fn name(&self) -> &str {
            "Queue"
        }
        async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.targets.lock().unwrap().pop_front().expect("selection queue exhausted"))
        }
        async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
            DecodeResult::Unknown { is_emergency: false }
        }
        fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
            None
        }
    }

    /// Gate double: admits the first external INVITE, rejects everything after
    /// — the shape of a capacity gate that filled up between two transmissions.
    #[derive(Default)]
    struct AdmitOnceGate {
        used: AtomicBool,
        tries: AtomicU32,
    }

    impl ProxySelfGate for AdmitOnceGate {
        fn try_admit_external(&self) -> AdmitDecision {
            self.tries.fetch_add(1, Ordering::SeqCst);
            if self.used.swap(true, Ordering::SeqCst) {
                AdmitDecision { admit: false, reason: Some("proxy_overload_cps".into()), retry_after_sec: 3 }
            } else {
                AdmitDecision::admit()
            }
        }
    }

    struct Fixture {
        core: ProxyCore,
        strategy: Arc<QueueStrategy>,
        gate: Arc<AdmitOnceGate>,
        metrics: Arc<ProxyMetrics>,
    }

    async fn fixture(targets: &[ProxyAddr]) -> Fixture {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy = QueueStrategy::of(targets);
        let gate = Arc::new(AdmitOnceGate::default());
        let metrics = Arc::new(ProxyMetrics::new());
        let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy.clone(), reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .self_gate(gate.clone())
            .build(ep);
        Fixture { core, strategy, gate, metrics }
    }

    fn parse_req(raw: &str) -> SipMessage {
        CustomParser::default().parse(raw.as_bytes()).unwrap()
    }

    fn invite(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
        parse_req(&format!(
            "INVITE sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag={from_tag}\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} INVITE\r\n\
Contact: <sip:alice@{UAC}:5060>\r\n\
Content-Length: 0\r\n\r\n"
        ))
    }

    fn cancel(call_id: &str, from_tag: &str, cseq: u32, branch: &str) -> SipMessage {
        parse_req(&format!(
            "CANCEL sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch={branch};rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag={from_tag}\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: {call_id}\r\n\
CSeq: {cseq} CANCEL\r\n\
Content-Length: 0\r\n\r\n"
        ))
    }

    fn src() -> std::net::SocketAddr {
        format!("{UAC}:5060").parse().unwrap()
    }

    // The finding this guards: an INVITE retransmission re-ran the strategy
    // while reusing the memoized branch — under a changed candidate set the
    // same branch went to a DIFFERENT worker, splitting one transaction into
    // two B2BUA calls. The retransmit must repeat the original forward.
    #[tokio::test(start_paused = true)]
    async fn retransmitted_invite_repeats_the_original_selection() {
        let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
        let req = invite("split-1@test", "tag-a", 1, "z9hG4bKr1");

        let first = f.core.route_request(&req, src()).await;
        let retx = f.core.route_request(&req, src()).await;

        assert_eq!(first.target, Some(ProxyAddr::new(W1, 5060)));
        assert_eq!(retx.target, Some(ProxyAddr::new(W1, 5060)), "retransmit must NOT be re-routed to w2");
        assert_eq!(f.strategy.calls.load(Ordering::SeqCst), 1, "the strategy must not run again for a retransmit");
    }

    // A retransmit of an admitted INVITE must bypass the gate (no 503 to a
    // setup already ringing downstream) and not inflate sip_proxy_calls_total.
    #[tokio::test(start_paused = true)]
    async fn retransmitted_invite_is_not_regated_or_recounted() {
        let f = fixture(&[ProxyAddr::new(W1, 5060)]).await;
        let req = invite("regate-1@test", "tag-a", 1, "z9hG4bKr2");

        let first = f.core.route_request(&req, src()).await;
        let retx = f.core.route_request(&req, src()).await;

        assert_eq!(first.decision, RoutingDecisionKind::SelectNew);
        assert_ne!(retx.decision, RoutingDecisionKind::Reject, "gate must not 503 a retransmission");
        assert_eq!(retx.target, Some(ProxyAddr::new(W1, 5060)));
        assert_eq!(f.gate.tries.load(Ordering::SeqCst), 1, "gate consulted once per transaction");
        assert_eq!(f.metrics.calls_total(), 1, "one call, not one per transmission");
    }

    // The finding this guards: the old 32 s entry TTL was far below the legal
    // ringing window (B2BUA SetupTimeout / sip-txn INVITE_INITIAL_TIMEOUT), so
    // a CANCEL after half a minute of ringing was forwarded via fresh selection
    // with a fresh branch → downstream 481 and the callee kept ringing.
    #[tokio::test(start_paused = true)]
    async fn cancel_after_a_minute_of_ringing_still_follows_the_invite() {
        let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
        let inv = invite("longring-1@test", "tag-a", 7, "z9hG4bKr3");
        f.core.route_request(&inv, src()).await;

        tokio::time::advance(Duration::from_secs(60)).await;

        let cxl = cancel("longring-1@test", "tag-a", 7, "z9hG4bKr3c");
        let outcome = f.core.route_request(&cxl, src()).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::Cancel);
        assert_eq!(outcome.target, Some(ProxyAddr::new(W1, 5060)), "CANCEL must follow the INVITE, not re-select");
        assert_eq!(f.strategy.calls.load(Ordering::SeqCst), 1, "no fallback selection for the CANCEL");
    }

    // Same window for the response side: a late non-2xx final must still find
    // the entry and get its hop-by-hop ACK synthesized toward the worker.
    #[tokio::test(start_paused = true)]
    async fn late_non_2xx_final_still_gets_ack_synthesized() {
        let f = fixture(&[ProxyAddr::new(W1, 5060)]).await;
        let inv = invite("latefinal-1@test", "tag-a", 9, "z9hG4bKr4");
        f.core.route_request(&inv, src()).await;

        tokio::time::advance(Duration::from_secs(60)).await;

        let raw = format!(
            "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKr4\r\n\
From: <sip:alice@{UAC}>;tag=tag-a\r\n\
To: <sip:bob@10.0.0.50>;tag=callee-1\r\n\
Call-ID: latefinal-1@test\r\n\
CSeq: 9 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
            panic!("expected response")
        };
        f.core.handle_response(resp).await;
        assert_eq!(f.metrics.ack_synthesized_total(), 1, "late 486 must still get the proxy ACK");
    }

    // The finding this guards: the entry key had no From-tag, so the two
    // directions of one Call-ID (independent CSeq spaces, both remembered
    // here) overwrote each other when their CSeq numbers coincided, and a
    // CANCEL was then forwarded to the wrong party with the wrong branch.
    #[tokio::test(start_paused = true)]
    async fn same_callid_same_cseq_different_from_tags_do_not_collide() {
        let f = fixture(&[ProxyAddr::new(W1, 5060), ProxyAddr::new(W2, 5060)]).await;
        let dir_a = invite("glare-1@test", "tag-a", 5, "z9hG4bKa");
        let dir_b = invite("glare-1@test", "tag-b", 5, "z9hG4bKb");
        f.core.route_request(&dir_a, src()).await;
        f.core.route_request(&dir_b, src()).await;

        let cxl_a = cancel("glare-1@test", "tag-a", 5, "z9hG4bKac");
        let cxl_b = cancel("glare-1@test", "tag-b", 5, "z9hG4bKbc");
        let out_a = f.core.route_request(&cxl_a, src()).await;
        let out_b = f.core.route_request(&cxl_b, src()).await;

        assert_eq!(out_a.target, Some(ProxyAddr::new(W1, 5060)), "direction A's CANCEL follows A's INVITE");
        assert_eq!(out_b.target, Some(ProxyAddr::new(W2, 5060)), "direction B's CANCEL follows B's INVITE");
    }
}

#[cfg(test)]
mod cookie_identity_tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::types::BindUdpOpts;
    use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

    use crate::addr::ProxyAddr;
    use crate::core::{ProxyCore, ProxyCoreBuilder};
    use crate::observability::metrics::RoutingDecisionKind;
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::{WorkerEntry, WorkerRegistry};
    use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

    const W1_POD: &str = "10.244.5.8";
    const UAC: &str = "10.244.7.13";
    const PROXY_VIP: &str = "172.20.255.250";
    const SNAT_NODE: &str = "172.20.0.11";

    /// Strategy double that records the address handed to `encode_stickiness`
    /// — the contract under test: for a worker-originated dialog-creating
    /// request it must be the worker's REGISTRY identity, not the UDP source.
    #[derive(Default)]
    struct CookieCaptureStrategy {
        encoded_for: Mutex<Option<ProxyAddr>>,
    }

    #[async_trait]
    impl RoutingStrategy for CookieCaptureStrategy {
        fn name(&self) -> &str {
            "CookieCapture"
        }
        async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
            Err(SelectError::NoTarget { reason: "worker-outbound tests never select".into() })
        }
        async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
            DecodeResult::Unknown { is_emergency: false }
        }
        fn encode_stickiness(&self, target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
            *self.encoded_for.lock().unwrap() = Some(target.clone());
            None
        }
    }

    struct Fixture {
        core: ProxyCore,
        strategy: Arc<CookieCaptureStrategy>,
    }

    async fn fixture() -> Fixture {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy = Arc::new(CookieCaptureStrategy::default());
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy.clone(), reg)
            .clock(Clock::test_at(0))
            .build(ep);
        Fixture { core, strategy }
    }

    /// A b-leg INVITE: worker-originated (top Via = the worker), dialog-creating
    /// (no To-tag), R-URI = the callee.
    fn bleg_invite(top_via_host: &str) -> SipMessage {
        let raw = format!(
            "INVITE sip:sipp@{UAC}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {top_via_host}:5060;branch=z9hG4bKbleg;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>\r\n\
Call-ID: bleg-1@{UAC}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:b2bua@{top_via_host}:5060;leg=b>\r\n\
Content-Length: 0\r\n\r\n"
        );
        CustomParser::default().parse(raw.as_bytes()).unwrap()
    }

    // Regression for the b-leg long-call-loss class: behind the keepalived VIP
    // the worker's INVITE arrives with src = SNAT node IP + ephemeral port. The
    // cookie must be encoded for the worker's REGISTRY identity (the SNAT-immune
    // top-Via sent-by) — encoding it for `src` made encode_stickiness miss the
    // registry and emit a param-less cookie RR, so the callee's later in-dialog
    // requests decoded Unknown and were re-sharded to an arbitrary worker.
    #[tokio::test]
    async fn snat_masqueraded_bleg_invite_encodes_cookie_for_the_via_worker() {
        let f = fixture().await;
        let req = bleg_invite(W1_POD);
        let snat_src = format!("{SNAT_NODE}:63522").parse().unwrap();

        let outcome = f.core.route_request(&req, snat_src).await;

        assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
        assert_eq!(
            *f.strategy.encoded_for.lock().unwrap(),
            Some(ProxyAddr::new(W1_POD, 5060)),
            "cookie must carry the worker's registry identity, not the SNAT source"
        );
    }

    // Pod-direct fallback: the top Via names an unregistered host (e.g. a
    // just-rebooted pod) but the UDP source IS the registered worker — the
    // cookie falls back to the source identity, as before.
    #[tokio::test]
    async fn pod_direct_source_remains_the_cookie_fallback() {
        let f = fixture().await;
        let req = bleg_invite("10.244.9.99"); // not in the registry
        let pod_src = format!("{W1_POD}:5060").parse().unwrap();

        let outcome = f.core.route_request(&req, pod_src).await;

        assert_eq!(outcome.decision, RoutingDecisionKind::WorkerOutbound);
        assert_eq!(
            *f.strategy.encoded_for.lock().unwrap(),
            Some(ProxyAddr::new(W1_POD, 5060)),
            "un-NAT'd worker source still identifies the cookie"
        );
    }
}

#[cfg(test)]
mod rfc_small_fix_tests {
    use std::sync::Arc;

    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::types::BindUdpOpts;
    use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

    use crate::addr::ProxyAddr;
    use crate::core::{ProxyCore, ProxyCoreBuilder};
    use crate::observability::metrics::RoutingDecisionKind;
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::WorkerRegistry;
    use crate::strategies::forward_all::ForwardAllStrategy;
    use crate::{ProxyMetrics, RoutingStrategy};

    const UAC: &str = "10.244.7.13";
    const PROXY_VIP: &str = "172.20.255.250";
    const W1: &str = "10.0.0.1";

    async fn core_with_metrics() -> (ProxyCore, Arc<ProxyMetrics>) {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
        let metrics = Arc::new(ProxyMetrics::new());
        let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .build(ep);
        (core, metrics)
    }

    fn parse_req(raw: &str) -> SipMessage {
        CustomParser::default().parse(raw.as_bytes()).unwrap()
    }

    // §16.3 check 2: an ACK with Max-Forwards: 0 is silently discarded — a 483
    // (or any response) to an ACK is a protocol violation.
    #[tokio::test]
    async fn ack_at_max_forwards_zero_is_dropped_silently() {
        let (core, metrics) = core_with_metrics().await;
        let req = parse_req(&format!(
            "ACK sip:bob@10.0.0.50:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKmf0;rport\r\n\
Max-Forwards: 0\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: mf0-1@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
        ));
        let before = metrics.messages_total();
        let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
        assert_eq!(outcome.decision, RoutingDecisionKind::Reject);
        assert_eq!(metrics.messages_total(), before, "no response (and no forward) may be generated for the ACK");
    }

    // §7.3.1: a UA may fold its route set into ONE comma-combined Route header.
    // Stripping our own entry must pop only that entry — deleting the whole
    // line dropped the downstream proxy's Route and bypassed its route set.
    #[tokio::test]
    async fn folded_route_strip_preserves_the_downstream_route() {
        let (core, _metrics) = core_with_metrics().await;
        let req = parse_req(&format!(
            "BYE sip:b2bua@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKfold;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: fold-1@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:5060;lr>, <sip:10.9.9.9:5062;lr>\r\n\
Content-Length: 0\r\n\r\n"
        ));
        let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
        assert_eq!(
            outcome.decision,
            RoutingDecisionKind::LooseRoute,
            "the surviving downstream Route entry must drive loose routing"
        );
        assert_eq!(outcome.target, Some(ProxyAddr::new("10.9.9.9", 5062)));
    }

    // An out-of-range URI port (70596 & 0xFFFF == 5060) must read as malformed,
    // never silently truncate into an alias of the proxy's own address.
    #[tokio::test]
    async fn oversized_route_port_does_not_alias_the_advertised_address() {
        let (core, _metrics) = core_with_metrics().await;
        let req = parse_req(&format!(
            "BYE sip:b2bua@{W1}:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKbig;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>;tag=u\r\n\
Call-ID: bigport-1@test\r\n\
CSeq: 2 BYE\r\n\
Route: <sip:{PROXY_VIP}:70596;lr>\r\n\
Content-Length: 0\r\n\r\n"
        ));
        let outcome = core.route_request(&req, format!("{UAC}:5060").parse().unwrap()).await;
        // 70596 used to wrap to 5060, match the advertised VIP, and be stripped
        // as a self-route. It must instead stay foreign (and being malformed,
        // it can't drive loose routing either) → plain selection.
        assert_eq!(outcome.decision, RoutingDecisionKind::SelectNew);
    }
}
