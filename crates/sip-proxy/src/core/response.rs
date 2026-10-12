//! Response path, single-endpoint: validate ≥2 Via and that the top is us;
//! route to the next Via (received/rport precedence); reverse-path failover of
//! an INVITE response to the backup of the cookie naming the destination worker
//! when that worker is confirmed Dead; pop the top Via entry (comma-aware);
//! forward; remember the ACK-relay hop for a non-2xx INVITE final — the node
//! the final arrived from (the ACK itself travels end-to-end — see
//! `core/request`).
//!
//! Every response is relayed, a 100 included: §16.7's absorption of a 100 is a
//! stateful proxy's, and this proxy is stateless (§16.11), so it neither
//! absorbs the downstream 100 nor generates one of its own.
//!
//! An address that LEFT the worker set counts as `Dead` here for Timer H
//! (`registry::tombstone`): a node whose endpoint is withdrawn while its process
//! still serves must not take an INVITE response back, so that response follows
//! the cookie's backup. A non-INVITE response answers a client transaction only
//! its sender holds (§17.1.2) and follows the Via whatever the sender's health.

use sip_message::header::Via;
use sip_message::types::SipResponse;
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{ack_hop_key, invite_txn_key};
use crate::observability::metrics::{Direction, MessageResult};
use crate::registry::WorkerHealth;
use crate::strategy::DecodeResult;
use crate::trace::emit;

use super::ProxyCore;

/// Whether a response this hop relayed rejects the call's SETUP for good — the
/// class after which the ACK is the only fact left. The two auth challenges are
/// not in it: they are answered by retrying the same call with credentials, so
/// the call outlives the transaction they end.
fn rejects_the_setup(status: u16, method: &Method) -> bool {
    *method == Method::Invite && (300..700).contains(&status) && !matches!(status, 401 | 407)
}

impl ProxyCore {
    /// `src` is the datagram's source: the node that sent this final, which
    /// is the hop a §17.1.1.3 ACK for it must reach.
    pub(super) async fn handle_response(&self, resp: SipResponse, src: std::net::SocketAddr) {
        let cseq = resp.cseq();
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        self.metrics.record_response(cseq.method().as_str(), resp.status());
        // Per-call trace tier (ADR-0026): the parsed Call-ID, one predicted
        // branch while nothing is sampled.
        let at_ms = self.now_ms() as i64;
        let is_traced = emit::response_in(
            &self.traces,
            resp.call_id().as_str(),
            at_ms,
            resp.status(),
            cseq.method().as_str(),
            resp.image(),
        );
        // The relay below consumes the message, so a TRACED call's key is
        // carried across it. An untraced call allocates nothing — the emission
        // above already answered whether this call has a span, so the clone
        // rides that answer and not the process-wide sampled flag.
        let traced: Option<String> = is_traced.then(|| resp.call_id().as_str().to_string());
        let ends_the_call = cseq.method() == Method::Bye && resp.status() >= 200;

        // §16.7.3: need ≥2 Via (ours + the next hop's).
        let hops: Vec<Via> = resp.via().iter().cloned().collect();
        if hops.len() < 2 {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }
        let (top_host, top_port) = hops[0].host_port();
        // Dual-face: the proxy stamps its outbound Via with the EGRESS face's
        // advertise, so a response legitimately names either face here.
        if !self.is_self_addr(top_host, top_port) {
            // Top Via is not us — not our response to relay.
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        let next = &hops[1];
        // received / rport take precedence over sent-by (§18.2.2 / §16.7.3).
        let (next_host, next_port) = next.response_target();
        let mut host = next_host.to_string();
        let mut port = next_port;

        // ── Reverse-path failover: INVITE responses only ────────────────────
        // An INVITE response from a **confirmed Dead** next-Via worker answers a
        // setup the cookie's `w_bak` materialises, so it goes there. A non-INVITE
        // response answers a client transaction only its sender holds (§17.1.2):
        // no other node can consume it, so it follows the Via (§16.7 step 6) and
        // is lost with its sender rather than dropped here.
        //
        // A worker that is merely `Unknown` (a freshly-rebooted pod the health
        // probe has not re-confirmed yet), `NotReady` or `Draining` is still up and
        // owns its transactions, so its responses reach it. (Request-path
        // routing-around a non-Alive worker is separate; that lives in the
        // strategy's `decode_stickiness`.)
        //
        // The worker is IDENTIFIED by its Via **sent-by** — its advertised
        // registry address, SNAT-immune (the same signal the request path keys
        // worker-outbound classification on). The received/rport-derived
        // `host:port` above stays the SEND target per §18.2.2/RFC 3581, but it
        // must not be the lookup key: behind the keepalived VIP it is the SNAT
        // node IP + an ephemeral port, which matches no registry entry. A
        // departed address answers `Dead` from the registry's tombstone, so
        // this branch covers it too.
        let (sent_by_host, sent_by_port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(sent_by_host, sent_by_port);
        let dead_worker = (cseq.method() == Method::Invite)
            .then(|| self.registry.lookup_by_address(&sent_by))
            .flatten()
            .filter(|dest| dest.health == WorkerHealth::Dead);
        if let Some(dead) = dead_worker {
            match self.own_cookie_for(&resp, &dead.id) {
                Some(params) => match self
                    .strategy
                    .decode_stickiness(&params, &SipMessage::Response(resp.clone()))
                    .await
                {
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

        // Pop our own Via entry (§7.3.1 comma-aware) and forward. The thawed
        // draft reads nothing below that top line, and freezing renders the
        // relayed datagram once.
        let Ok(popped) = resp.thaw().pop_top::<Via>() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let Ok(bytes) = popped.freeze_bytes() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let next_hop = ProxyAddr::new(host, port);
        self.send_to(&bytes, &next_hop).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);
        if let Some(call_id) = &traced {
            emit::relayed(&self.traces, call_id, at_ms, &next_hop, &bytes);
            // The BYE's final is the last fact this hop will see about the
            // call: close the span here rather than leaving it to the TTL.
            if ends_the_call {
                self.traces.close(call_id);
            }
            // A non-2xx INVITE final ends the CALL only when it rejects its
            // setup for good. An auth challenge does not — the caller retries
            // the same Call-ID with credentials — and neither does a
            // mid-dialog re-INVITE's 488/491, which the registry tells apart
            // by the transaction it is holding. One more fact follows a
            // rejection on this hop, the ACK, so the span closes there.
            if rejects_the_setup(resp.status(), cseq.method()) {
                self.traces.arm_close_on_ack(call_id, resp.from().tag(), cseq.seq());
            }
        }

        // ── Relayed non-2xx INVITE final: remember the ACK-relay hop ────────
        // This transaction-less proxy (ADR-0022 X4) synthesizes no §17.1.1.3
        // hop ACK: the UAS retransmits the final through this relay until the
        // upstream's own ACK arrives, which the request path relays on the
        // memo written here.
        //
        // The memo carries the hop to repeat: the node the final ARRIVED from
        // — after a failover that is the survivor, not the INVITE's target.
        // The branch the sender's server transaction is keyed on is §16.11's
        // function of the ACK itself (`crate::branch`). Gating the relay on
        // the memo (not the INVITE entry alone) keeps a takeover worker's 2xx
        // ACK safe when its reset `IdGen` re-mints a branch aliasing the dead
        // primary's INVITE (see `core/request`). Short TTL: the upstream ACKs
        // within its final-retransmit window (a re-sent final refreshes it).
        if (300..700).contains(&resp.status()) && cseq.method() == Method::Invite {
            // The response echoes the request's From (tag included), and the
            // Via below ours is the upstream's as the INVITE carried it, so
            // this re-builds exactly the key the INVITE was remembered under.
            let call_id = resp.call_id();
            let from = resp.from();
            let key = invite_txn_key(next, call_id.as_str(), from.tag(), cseq.seq());
            if let Some(found) = self.cancel_lru.lookup(&key) {
                self.cancel_lru.remember(
                    &ack_hop_key(next, call_id.as_str(), from.tag(), cseq.seq()),
                    crate::cancel_lru::CancelEntry {
                        target: ProxyAddr::from(src),
                        branch: found.branch.clone(),
                        stickiness: None,
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }
    }

    /// The stickiness cookie for reverse-path failover toward `worker`: the
    /// bottommost of the proxy's own Record-Route entries (either face) whose
    /// cookie names `worker` as primary. Why the bottommost: ADR-0009 X1.
    fn own_cookie_for(
        &self,
        resp: &SipResponse,
        worker: &str,
    ) -> Option<crate::strategy::RouteParams> {
        resp.list::<sip_message::header::RecordRouteEntry>()
            .ok()?
            .iter()
            .rev()
            .filter(|entry| {
                let (host, port) = entry.uri().host_port();
                self.is_self_addr(host, port)
            })
            .map(|entry| crate::headers::cookie_params(entry.uri()))
            .find(|params| self.strategy.stickiness_primary(params) == Some(worker))
    }
}

#[cfg(test)]
mod reverse_failover_tests;

#[cfg(test)]
mod hop_by_hop_tests;
