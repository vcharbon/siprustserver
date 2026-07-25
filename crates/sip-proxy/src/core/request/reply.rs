//! Self-generated UAS finals: the synthesized response to the packet source,
//! the `ackhop|` absorb memo the proxy writes when it becomes the UAS of a
//! non-2xx INVITE transaction, and the select-failure → 503 mapping.
//! Relayed responses do NOT live here — see `core/response`.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{serialize, SipMessage, SipRequest};

use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::strategy::SelectError;

use super::super::ProxyCore;
use super::{top_via_branch, RouteOutcome};

impl ProxyCore {
    /// Synthesize a UAS response to the source.
    pub(super) async fn reply(&self, req: &SipRequest, src: SocketAddr, status: u16, reason: &str, extra: &[SipHeader]) {
        let opts = GenerateResponseOpts {
            to_tag: Some(self.id_gen.new_tag()),
            extra_headers: extra.to_vec(),
            ..Default::default()
        };
        let resp = generate_response(req, status, reason, &opts);
        self.reply_to_source(&serialize(&SipMessage::Response(resp)), src).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Responded);

        // ── §16.7 / §17.1.1.3: absorb the ACK to our OWN non-2xx INVITE final ─
        // Generating a final response makes this proxy the UAS of that INVITE
        // transaction, and its non-2xx ACK is hop-by-hop — the upstream's ACK
        // terminates HERE. Write the `ackhop|` memo (see `cancel_lru.rs`) with
        // an EMPTY `branch`, which is what tells the request path to absorb
        // the matching ACK rather than relay it (no downstream exists for a
        // self-generated reject; relaying would run the strategy and hand a
        // worker a stray ACK matching no transaction it ever created).
        if (300..700).contains(&status) && req.method.as_str() == "INVITE" {
            if let Some(upstream_branch) = top_via_branch(req) {
                self.cancel_lru.remember(
                    &crate::cancel_lru::ack_hop_key(
                        &req.call_id,
                        req.from.tag.as_deref(),
                        req.cseq.seq,
                    ),
                    crate::cancel_lru::CancelEntry {
                        target: self.advertised.clone(),
                        branch: String::new(),
                        upstream_branch,
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }
    }

    /// Map a `select_for_new_dialog` failure to its 503. Each variant carries a
    /// distinct `Retry-After` + `Reason` text so the UAC (and dashboards) can
    /// tell "no worker at all" from "this worker is being rate-capped".
    pub(super) async fn reply_select_failure(&self, req: &SipRequest, src: SocketAddr, err: SelectError) -> RouteOutcome {
        let (retry_after, reason_text) = match &err {
            SelectError::NoTarget { .. } => (5u32, "no_target_available".to_string()),
            SelectError::RateCapExhausted { retry_after_sec, .. } => {
                (*retry_after_sec, "worker_rate_capped".to_string())
            }
        };
        let extra = [
            SipHeader { name: "Retry-After".into(), value: retry_after.to_string().into() },
            SipHeader { name: "Reason".into(), value: format!("SIP;cause=503;text=\"{reason_text}\"").into() },
        ];
        self.reply(req, src, 503, "Service Unavailable", &extra).await;
        RouteOutcome { decision: RoutingDecisionKind::Reject, target: None }
    }
}
