//! Request path — port of `handleRequestImpl` (ProxyCore.ts L623-1133),
//! single-endpoint. Max-Forwards → 483; hop-by-hop ACK absorption; top-Route
//! strip + worker-outbound classification; (stubbed) self-gate; target
//! selection (CANCEL LRU → loose-route next hop → worker-outbound R-URI →
//! cookie decode → select); received/rport stamping; Record-Route insertion;
//! Via push; LRU remember; serialize + forward.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::{is_emergency_request, parse_sip_uri};
use sip_message::types::SipHeader;
use sip_message::{serialize, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{call_id_cseq_key, CancelEntry};
use crate::headers::{
    build_record_route_value, first_header_value, populate_received_rport_on_top_via, prepend_header,
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

impl ProxyCore {
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
        // Source-based worker-outbound override — break the in-dialog loop.
        if !is_worker_outbound && self.registry.lookup_by_address(&ProxyAddr::from(src)).is_some() {
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

        let our_branch = reuse_branch.unwrap_or_else(|| self.id_gen.new_branch());
        let via_value =
            format!("SIP/2.0/UDP {}:{};branch={};rport", self.advertised.host, self.advertised.port, our_branch);
        prepend_header(&mut headers, "Via", &via_value);

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
