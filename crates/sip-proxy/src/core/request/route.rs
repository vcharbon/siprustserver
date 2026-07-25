//! The §16 routing ladder for one inbound request: preflight checks, the
//! non-2xx ACK hop decision, Route preprocessing + worker-outbound
//! classification, self-gate admission, target selection, retransmission
//! branch reuse, Via push, LRU memos, serialize + forward.
//! Record-Route insertion lives in [`record_route`](super::record_route);
//! self-generated finals in [`reply`](super::reply).

use std::net::SocketAddr;

use sip_message::message_helpers::{is_emergency_request, parse_sip_uri};
use sip_message::types::SipHeader;
use sip_message::{serialize_request_parts, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{call_id_cseq_key, CancelEntry};
use crate::headers::{first_header_value, populate_received_rport_on_top_via, prepend_header, upsert_header, via_sent_by_addr};
use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::self_gate::BypassKind;
use crate::strategy::{DecodeResult, SelectOpts};

use super::super::{is_dialog_creating, ProxyCore};
use super::{top_via_branch, RouteOutcome};

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

    pub(in crate::core) async fn route_request(&self, msg: &SipMessage, src: SocketAddr) -> RouteOutcome {
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

        // ── §16.3 check 5: Proxy-Require ────────────────────────────────────
        // This proxy supports no proxy extensions, so any option-tag in
        // Proxy-Require is unsupported: reject with 420 (Bad Extension) and an
        // Unsupported header listing the offending tags — the request MUST NOT
        // be forwarded. ACK is exempt: a proxy never answers an ACK (it
        // terminates a transaction; nothing upstream awaits a reply), so an ACK
        // with an unsupported Proxy-Require is silently dropped, never 420'd.
        if let Some(pr) = first_header_value(&req.headers, "proxy-require") {
            let unsupported: Vec<String> = crate::headers::split_top_level_commas(pr)
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if !unsupported.is_empty() {
                if method == "ACK" {
                    return RouteOutcome { decision: RoutingDecisionKind::Reject, target: None };
                }
                let extra = [SipHeader {
                    name: "Unsupported".into(),
                    value: unsupported.join(", ").into(),
                }];
                self.reply(req, src, 420, "Bad Extension", &extra).await;
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
        // remembered INVITE) also keeps a takeover worker's 2xx ACK safe: the
        // simulated fabric's per-worker `IdGen` resets on failover, so its
        // fresh ACK branch can ALIAS the dead primary's INVITE branch — but no
        // non-2xx final was ever relayed for a confirmed call, so no memo
        // exists and the ACK flows.
        //
        // What a match means depends on who generated the final:
        //  • relayed final (memo carries the INVITE's forward) — RELAY the ACK
        //    on that exact hop, same target + same outbound branch, so the
        //    downstream server transaction (§17.2.3) matches it and stops
        //    retransmitting the final (Timer G). This transaction-less proxy
        //    never synthesizes the hop ACK itself (ADR-0022 X4; see
        //    `core/response.rs`) — the upstream's ACK is the only quench,
        //    end-to-end, which is what keeps a lossy caller hop recoverable.
        //  • the proxy's OWN final (memo written by `reply()`, empty `branch`:
        //    no downstream exists) — the ACK terminates HERE: absorb it, never
        //    run the strategy over it and hand a worker a stray ACK matching
        //    no transaction it ever created.
        let mut ack_hop: Option<CancelEntry> = None;
        if method == "ACK" {
            let key = crate::cancel_lru::ack_hop_key(&req.call_id, req.from.tag.as_deref(), req.cseq.seq);
            if let Some(found) = self.cancel_lru.lookup(&key) {
                let same_txn = !found.upstream_branch.is_empty()
                    && top_via_branch(req).as_deref() == Some(found.upstream_branch.as_str());
                // Branchless legacy fallback (pre-RFC-3261 upstream): no branch
                // to compare, so keep the old route-less heuristic for it — a
                // 2xx ACK would carry the dialog's Route set.
                let legacy_routeless = found.upstream_branch.is_empty()
                    && first_header_value(&req.headers, "route").is_none();
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
            // Dual-face: a self Route may carry EITHER face's advertise (the
            // §16.6 double-RR stamps the two entries with the face facing each
            // party), so match both — the in-dialog request must pop BOTH self
            // entries before the next-hop decision.
            let self_route = crate::headers::uri_port_u16(parsed.port)
                .is_some_and(|p| self.is_self_addr(&parsed.host, p));
            if !self_route {
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

        // ── Proxy-self gate (ELU/CPS admission) ─────────────────────────────
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
                        SipHeader { name: "Retry-After".into(), value: decision.retry_after_sec.to_string().into() },
                        SipHeader { name: "Reason".into(), value: format!("SIP;cause=503;text=\"{reason}\"").into() },
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
        } else if let Some(found) = ack_hop {
            // The §17.1.1.3 ACK for a relayed non-2xx final: repeat the
            // INVITE's forward exactly (see the hop-decision block above).
            target = Some(found.target);
            reuse_branch = Some(found.branch);
            decision = RoutingDecisionKind::AckHop;
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

        self.insert_double_record_route(&mut headers, msg, req, src, &target, is_worker_outbound, &via_worker_addr);

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
        // Stamp the EGRESS face's advertise on the pushed Via (§18.1.1: the
        // sent-by must be an address the next hop can return the response to —
        // in dual-face mode that is the face the request leaves on, so the
        // response arrives back on the same face).
        let egress_adv = self.egress_advertised(&target);
        let via_value =
            format!("SIP/2.0/UDP {}:{};branch={};rport", egress_adv.host, egress_adv.port, our_branch);
        prepend_header(&mut headers, "Via", &via_value);

        // Remember the outbound (target, branch) so a retransmit of THIS
        // request repeats the forward. Short TTL: retransmits stop at Timer B/F.
        if method != "CANCEL" {
            if let Some(k) = &rtx_key {
                self.cancel_lru.remember(
                    k,
                    CancelEntry {
                        target: target.clone(),
                        branch: our_branch.clone(),
                        upstream_branch: incoming_branch.clone().unwrap_or_default(),
                    },
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
                CancelEntry {
                    target: target.clone(),
                    branch: our_branch,
                    upstream_branch: incoming_branch.clone().unwrap_or_default(),
                },
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
}
