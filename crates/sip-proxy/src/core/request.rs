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
use sip_message::{serialize, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{call_id_cseq_key, CancelEntry};
use crate::headers::{
    build_record_route_value, first_header_value, populate_received_rport_on_top_via, prepend_header, via_sent_by_addr,
    remove_first_header, upsert_header,
};
use crate::observability::logger::RoutingDecisionLog;
use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::self_gate::BypassKind;
use crate::strategy::{DecodeResult, SelectError, SelectOpts};

use super::{is_dialog_creating, ProxyCore};

/// Outcome of routing a request — what to log/meter after the work is done.
struct RouteOutcome {
    decision: RoutingDecisionKind,
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
    /// True if the request's top Via sent-by is one of our registered workers —
    /// i.e. the request was originated *by* a worker (e.g. a B2BUA in-dialog
    /// keepalive OPTIONS toward the far endpoint). SNAT-immune: the worker's
    /// advertised identity rides the message, unlike the UDP source which the VIP
    /// masquerades. Used as the worker-outbound discriminator on the request path.
    fn top_via_is_worker(&self, req: &SipRequest) -> bool {
        first_header_value(&req.headers, "via")
            .and_then(via_sent_by_addr)
            .map(|a| self.registry.lookup_by_address(&a).is_some())
            .unwrap_or(false)
    }

    pub(super) async fn handle_request(&self, req: SipRequest, src: SocketAddr) {
        let start_ms = self.now_ms();
        let method = req.method.to_ascii_uppercase();
        let call_id = req.call_id.clone();
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        self.metrics.record_request(&method);
        // A new call = an initial dialog-creating INVITE (no To-tag yet).
        if is_dialog_creating(&method) && method == "INVITE" && req.to.tag.is_none() {
            self.metrics.record_call();
        }

        let outcome = self.route_request(&req, &method, src).await;

        let duration = (self.now_ms().saturating_sub(start_ms)) as f64 / 1000.0;
        self.metrics.observe_routing_duration(duration);
        self.metrics.record_routing_decision(outcome.decision);
        self.logger.routing_decision(&RoutingDecisionLog {
            call_id,
            method,
            decision: decision_label(outcome.decision).to_string(),
            strategy: self.strategy.name().to_string(),
            target: outcome.target.as_ref().map(ProxyAddr::to_string),
        });
    }

    async fn route_request(&self, req: &SipRequest, method: &str, src: SocketAddr) -> RouteOutcome {
        let select = RoutingDecisionKind::SelectNew;

        // ── §16.3 + Max-Forwards ────────────────────────────────────────────
        let mf: i64 = first_header_value(&req.headers, "max-forwards")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(70);
        if mf <= 0 {
            self.reply(req, src, 483, "Too Many Hops", &[]).await;
            return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
        }
        let mf_next = mf - 1;

        // ── Hop-by-hop ACK absorption (§17.1.1.3) ───────────────────────────
        if method == "ACK" && first_header_value(&req.headers, "route").is_none() {
            let key = call_id_cseq_key(&req.call_id, req.cseq.seq);
            if self.cancel_lru.lookup(&key).is_some() {
                return RouteOutcome { decision: select, target: None };
            }
        }

        // ── §16.4 Route preprocessing ───────────────────────────────────────
        let mut headers: Vec<SipHeader> = req.headers.clone();
        let mut stripped_route_params: Option<crate::strategy::RouteParams> = None;
        let mut is_worker_outbound = false;
        if let Some(top_route) = first_header_value(&headers, "route") {
            if let Some(parsed) = parse_sip_uri(top_route) {
                if parsed.host == self.advertised.host && parsed.port as u16 == self.advertised.port {
                    let params = sip_message::message_helpers::parse_uri_params(top_route);
                    remove_first_header(&mut headers, "route");
                    if params.contains_key("outbound") {
                        is_worker_outbound = true;
                    } else {
                        stripped_route_params = Some(params);
                    }
                }
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
        if !is_worker_outbound
            && (self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some()
                || self.top_via_is_worker(req))
        {
            is_worker_outbound = true;
            stripped_route_params = None;
        }

        // ── Proxy-self gate (stubbed always-admit) ──────────────────────────
        let has_to_tag = req.to.tag.as_deref().is_some_and(|t| !t.is_empty());
        let is_new_dialog_invite = method == "INVITE" && !has_to_tag;
        let is_emergency = is_emergency_request(req);
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
            let key = call_id_cseq_key(&req.call_id, req.cseq.seq);
            if let Some(found) = self.cancel_lru.lookup(&key) {
                target = Some(found.target);
                reuse_branch = Some(found.branch);
                decision = RoutingDecisionKind::Cancel;
                self.metrics.record_cancel_lookup("hit");
            } else {
                self.metrics.record_cancel_lookup("miss");
                decision = RoutingDecisionKind::Cancel;
                match self.strategy.select_for_new_dialog(&SipMessage::Request(req.clone()), SelectOpts::default()).await {
                    Ok(t) => target = Some(t),
                    Err(e) => return self.reply_select_failure(req, src, e).await,
                }
            }
        } else if let Some(next) = loose_route_next_hop {
            target = Some(next);
            decision = RoutingDecisionKind::LooseRoute;
        } else if is_worker_outbound {
            match parse_sip_uri(&req.uri) {
                Some(parsed) => {
                    target = Some(ProxyAddr::new(parsed.host, parsed.port as u16));
                    decision = RoutingDecisionKind::WorkerOutbound;
                }
                None => {
                    self.reply(req, src, 400, "Bad Request", &[]).await;
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
            }
        } else if let Some(params) = &stripped_route_params {
            match self.strategy.decode_stickiness(params, &SipMessage::Request(req.clone())).await {
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
                    let opts = SelectOpts { emergency_override: is_emergency };
                    match self.strategy.select_for_new_dialog(&SipMessage::Request(req.clone()), opts).await {
                        Ok(t) => target = Some(t),
                        Err(e) => return self.reply_select_failure(req, src, e).await,
                    }
                    decision = RoutingDecisionKind::DecodeForward;
                }
            }
        } else {
            match self.strategy.select_for_new_dialog(&SipMessage::Request(req.clone()), SelectOpts::default()).await {
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

        if is_dialog_creating(method) {
            let cookie_addr = if is_worker_outbound { ProxyAddr::from(src) } else { target.clone() };
            let stickiness = self.strategy.encode_stickiness(&cookie_addr, &SipMessage::Request(req.clone()));
            let rr_value = match &stickiness {
                Some(params) => build_record_route_value(&self.advertised, params.iter()),
                None => build_record_route_value(&self.advertised, std::iter::empty()),
            };
            prepend_header(&mut headers, "Record-Route", &rr_value);
            self.metrics.record_route_inserted();
        }

        // ── §16.6 / §17.2.3 retransmission branch reuse ─────────────────────
        // Forward a *retransmission* with the SAME outbound top-Via branch the
        // original forward used, so the downstream transaction layer (and the
        // on-wire RFC 3261 §12.2.2 in-dialog-CSeq audit) correlate it to the
        // existing transaction instead of seeing a fresh transaction at the same
        // CSeq. Keyed on the full (Call-ID, upstream branch, method, CSeq) so a
        // genuine retransmit reuses the branch but a *different* request that
        // merely collides on the branch (a backup's reset `IdGen` after failover)
        // does not. Without this, a keepalive OPTIONS that retransmits before its
        // 200 lands reaches the callee as several distinct CSeq-N transactions and
        // the audit (correctly) rejects the 2nd as a CSeq reuse. CANCEL already
        // resolves its branch from the INVITE LRU above.
        let incoming_branch = top_via_branch(req);
        let rtx_key = incoming_branch
            .as_ref()
            .map(|b| retransmit_key(&req.call_id, b, &method, req.cseq.seq));
        if method != "CANCEL" && reuse_branch.is_none() {
            if let Some(k) = &rtx_key {
                if let Some(found) = self.cancel_lru.lookup(k) {
                    reuse_branch = Some(found.branch);
                }
            }
        }

        let our_branch = reuse_branch.unwrap_or_else(|| self.id_gen.new_branch());
        let via_value =
            format!("SIP/2.0/UDP {}:{};branch={};rport", self.advertised.host, self.advertised.port, our_branch);
        prepend_header(&mut headers, "Via", &via_value);

        // Remember the outbound branch so a retransmit of THIS request reuses it.
        if method != "CANCEL" {
            if let Some(k) = &rtx_key {
                self.cancel_lru.remember(
                    k,
                    CancelEntry { target: target.clone(), branch: our_branch.clone() },
                );
            }
        }

        if method == "INVITE" {
            let key = call_id_cseq_key(&req.call_id, req.cseq.seq);
            self.cancel_lru.remember(&key, CancelEntry { target: target.clone(), branch: our_branch });
            self.metrics.set_pending_invite_lru_size(self.cancel_lru.size() as u64);
        }

        // ── Serialize + forward ─────────────────────────────────────────────
        let mut out = req.clone();
        out.headers = headers;
        let bytes = serialize(&SipMessage::Request(out));
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

fn decision_label(kind: RoutingDecisionKind) -> &'static str {
    match kind {
        RoutingDecisionKind::SelectNew => "select_new",
        RoutingDecisionKind::DecodeForward => "decode_forward",
        RoutingDecisionKind::DecodeForwardBackup => "decode_forward_backup",
        RoutingDecisionKind::LooseRoute => "loose_route",
        RoutingDecisionKind::WorkerOutbound => "worker_outbound",
        RoutingDecisionKind::Cancel => "cancel",
        RoutingDecisionKind::Reject => "reject",
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
        let SipMessage::Request(req) = keepalive_options() else { unreachable!() };
        let outcome = core.route_request(&req, "OPTIONS", snat_src).await;

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

    // The un-NAT'd fast path still works: source IS the registered worker.
    #[tokio::test]
    async fn pod_direct_worker_source_is_still_worker_outbound() {
        let reg: Arc<dyn WorkerRegistry> =
            Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive("w1", ProxyAddr::new(W1_POD, 5060))]));
        let core = core(reg).await;

        let pod_src = format!("{W1_POD}:5060").parse().unwrap();
        let SipMessage::Request(req) = keepalive_options() else { unreachable!() };
        let outcome = core.route_request(&req, "OPTIONS", pod_src).await;

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
        let SipMessage::Request(req) = CustomParser::default().parse(raw.as_bytes()).unwrap() else { unreachable!() };
        let outcome = core.route_request(&req, "BYE", format!("{UAC}:5060").parse().unwrap()).await;

        assert_eq!(outcome.decision, RoutingDecisionKind::DecodeForward);
        assert_eq!(outcome.target, Some(ProxyAddr::new(W1_POD, 5060)));
    }

    // Reboot case (the reclaimed-long-call-loss root cause): a worker that has
    // just respawned onto a NEW pod IP the EndpointSlice informer has not yet
    // learned sends its keepalive with the `;outbound` marker the b2bua egress
    // now stamps (relay::apply_b_leg_egress). Neither the SNAT'd source nor the
    // top Via (the new IP) is a registered worker, so the registry-based
    // discriminators MISS — only the `;outbound` param can classify it. It must
    // still be worker-outbound and reach the UAC, not bounce back to a worker via
    // the stale cookie `target=`.
    #[tokio::test]
    async fn rebooted_worker_keepalive_with_outbound_marker_routes_to_uac() {
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
        let SipMessage::Request(req) = CustomParser::default().parse(raw.as_bytes()).unwrap() else { unreachable!() };
        // SNAT'd source (node IP) — also not a registered worker.
        let snat_src = "172.20.0.12:51000".parse().unwrap();
        let outcome = core.route_request(&req, "OPTIONS", snat_src).await;

        assert_eq!(
            outcome.decision,
            RoutingDecisionKind::WorkerOutbound,
            "a rebooted worker's keepalive must be worker-outbound via the ;outbound marker even when its new IP is absent from the registry"
        );
        assert_eq!(
            outcome.target,
            Some(ProxyAddr::new(UAC, 5060)),
            "the keepalive must reach the UAC (R-URI), not bounce back to a worker via the stale cookie target="
        );
    }
}
