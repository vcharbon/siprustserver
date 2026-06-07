//! Response path — port of `handleResponseImpl` (ProxyCore.ts L1246-1449),
//! single-endpoint. Validate ≥2 Via and that the top is us; route to the next
//! Via (received/rport precedence); reverse-path failover to the cookie's
//! `w_bak` when the destination worker is no longer alive; pop the top Via
//! entry (comma-aware); forward; synthesize a hop-by-hop ACK for a non-2xx
//! INVITE final.

use sip_message::generators::generate_proxy_ack_for_non_2xx;
use sip_message::message_helpers::{parse_sip_uri, parse_uri_params};
use sip_message::types::{ParamValue, SipResponse, Via};
use sip_message::{serialize, SipMessage};

use crate::addr::ProxyAddr;
use crate::cancel_lru::call_id_cseq_key;
use crate::headers::remove_first_header_entry;
use crate::observability::metrics::{Direction, MessageResult};
use crate::registry::WorkerHealth;
use crate::strategy::DecodeResult;

use super::ProxyCore;

fn param_str<'a>(via: &'a Via, name: &str) -> Option<&'a str> {
    match via.params.get(name) {
        Some(ParamValue::Value(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

impl ProxyCore {
    pub(super) async fn handle_response(&self, resp: SipResponse) {
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        self.metrics.record_response(&resp.cseq.method.as_str().to_ascii_uppercase(), resp.status);

        // §16.7.3: need ≥2 Via (ours + the next hop's).
        if resp.via.len() < 2 {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }
        let top = resp.via.first();
        let top_port = top.port.unwrap_or(5060);
        if top.host != self.advertised.host || top_port != self.advertised.port {
            // Top Via is not us — not our response to relay.
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        let next = resp.via.iter().nth(1).expect("len >= 2 checked");
        // received / rport take precedence over sent-by (§18.2.2 / §16.7.3).
        let mut host = param_str(next, "received").unwrap_or(&next.host).to_string();
        let mut port = param_str(next, "rport").and_then(|s| s.parse().ok()).unwrap_or(next.port.unwrap_or(5060));

        // ── Reverse-path failover ───────────────────────────────────────────
        // A response is the reply to an in-flight transaction the next-Via worker
        // is *waiting on* (e.g. the 200 to its own in-dialog keepalive OPTIONS).
        // Only reverse-fail it over to the cookie's `w_bak` when that worker is
        // **confirmed Dead** — NOT when it is merely `Unknown` (a freshly-rebooted
        // pod the health probe has not re-confirmed yet), `NotReady`, or `Draining`:
        // those are still up and own the transaction, so their response must reach
        // them. Failing a booting worker's keepalive-200 over to `w_bak` (which holds
        // no matching `KeepaliveTimeout`) let the worker's 5 s timeout fire and BYE
        // every call it had just reclaimed — the long-call-on-reboot teardown. A
        // draining worker also legitimately finishes in-flight calls, so it keeps its
        // responses too. (Request-path routing-around a non-Alive worker is separate;
        // that lives in the strategy's `decode_stickiness`.)
        if let Some(dest) = self.registry.lookup_by_address(&ProxyAddr::new(host.clone(), port)) {
            if dest.health == WorkerHealth::Dead {
                match self.find_own_record_route_params(&resp) {
                    Some(params) => match self.strategy.decode_stickiness(&params, &SipMessage::Response(resp.clone())).await {
                        DecodeResult::ForwardBackup { target, .. } => {
                            host = target.host;
                            port = target.port;
                        }
                        _ => {
                            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                            return;
                        }
                    },
                    None => {
                        self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                        return;
                    }
                }
            }
        }

        // Pop the top Via entry (comma-aware) and forward.
        let mut headers = resp.headers.clone();
        remove_first_header_entry(&mut headers, "via");
        let mut out = resp.clone();
        out.headers = headers;
        let next_hop = ProxyAddr::new(host, port);
        self.send_to(&serialize(&SipMessage::Response(out)), &next_hop).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);

        // ── Hop-by-hop ACK for a non-2xx INVITE final (§17.1.1.3) ───────────
        if (300..700).contains(&resp.status) && resp.cseq.method == "INVITE" {
            let key = call_id_cseq_key(&resp.call_id, resp.cseq.seq);
            if let Some(found) = self.cancel_lru.lookup(&key) {
                let ack = generate_proxy_ack_for_non_2xx(
                    &resp,
                    (&found.target.host, found.target.port),
                    &found.branch,
                    (&self.advertised.host, self.advertised.port),
                );
                self.send_to(&serialize(&SipMessage::Request(ack)), &found.target).await;
                self.metrics.record_ack_synthesized();
            }
        }
    }

    /// Extract the params of the proxy's own Record-Route entry from a response
    /// (echoed by the UAS per §16.6) — the stickiness cookie for reverse-path
    /// failover.
    fn find_own_record_route_params(&self, resp: &SipResponse) -> Option<crate::strategy::RouteParams> {
        for h in resp.headers.iter().filter(|h| h.name.eq_ignore_ascii_case("record-route")) {
            for entry in crate::headers::split_top_level_commas(&h.value) {
                if let Some(parsed) = parse_sip_uri(&entry) {
                    if parsed.host == self.advertised.host && parsed.port as u16 == self.advertised.port {
                        return Some(parse_uri_params(&entry));
                    }
                }
            }
        }
        None
    }
}
