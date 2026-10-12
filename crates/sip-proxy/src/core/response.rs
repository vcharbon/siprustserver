//! Response path, single-endpoint: validate ≥2 Via and that the top is us;
//! route to the next Via (received/rport precedence); reverse-path failover of
//! an INVITE response to the backup of the cookie naming the destination worker
//! (on this proxy's Record-Route, else on its own top Via), or to that worker's
//! new address, when the address it was sent to is confirmed Dead; pop the top
//! Via entry (comma-aware);
//! forward; remember the ACK-relay hop for a non-2xx INVITE final — the node
//! the final arrived from (the ACK itself travels end-to-end — see
//! `core/request`).
//!
//! A 100 is relayed like any other response: §16.7's absorption of a 100 is a
//! stateful proxy's, and this proxy is stateless (§16.11), so it neither
//! absorbs the downstream 100 nor generates one of its own. A response is
//! dropped only when its top Via is not this proxy, when it carries fewer than
//! two Vias, when it arrives on the external face and its next Via names no
//! registry worker (ADR-0048), when it answers an INVITE for a Dead worker and
//! no cookie of this proxy names a backup to fail it over to, or when its top
//! Via cannot be popped and the rest re-rendered.
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
use crate::observability::metrics::{Direction, Face, MessageResult, UntrustedClaim};
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
    /// is the hop a §17.1.1.3 ACK for it must reach. `face` is the face it
    /// arrived on.
    pub(super) async fn handle_response(
        &self,
        resp: SipResponse,
        src: std::net::SocketAddr,
        face: Face,
    ) {
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
        let (mut host, mut port) = match face {
            // received / rport take precedence over sent-by (§18.2.2 / §16.7.3).
            Face::Internal => {
                let (host, port) = next.response_target();
                (host.to_string(), port)
            }
            Face::External => match self.external_response_target(next) {
                Some(target) => (target.host, target.port),
                None => {
                    self.metrics.record_external_untrusted(UntrustedClaim::ResponseNotToWorker);
                    self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                    return;
                }
            },
        };

        // ── Reverse-path failover: INVITE responses only ────────────────────
        // An INVITE response from a **confirmed Dead** next-Via worker answers a
        // setup the cookie's `w_bak` materialises, so it goes there — unless
        // the worker that left the address is up again at a new one (a rebooted
        // pod that reclaimed its calls and their INVITEs, ADR-0014): the
        // response goes there, cookie or none (a 100 carries none). A non-INVITE
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
        // worker-outbound classification on). The send target above may be the
        // received/rport address (§18.2.2/RFC 3581), which must not be the
        // lookup key: behind address translation it is a node IP and an
        // ephemeral port, which matches no registry entry. A departed address
        // answers `Dead` from the registry's tombstone, so this branch covers
        // it too.
        let (sent_by_host, sent_by_port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(sent_by_host, sent_by_port);
        let dead_worker = (cseq.method() == Method::Invite)
            .then(|| self.registry.lookup_by_address(&sent_by))
            .flatten()
            .filter(|dest| dest.health == WorkerHealth::Dead);
        let moved = dead_worker
            .as_ref()
            .and_then(|dead| self.registry.resolve(&dead.id))
            .filter(|w| w.address != sent_by && w.health != WorkerHealth::Dead);
        if let Some(moved) = moved {
            host = moved.address.host;
            port = moved.address.port;
        } else if let Some(dead) = dead_worker {
            match self.own_cookie_for(&resp, &hops[0], &dead.id) {
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

    /// A test's `handle_response` on a single-face proxy, where every response
    /// arrives on the internal face. A dual-face test names the face itself.
    #[cfg(test)]
    pub(super) async fn handle_response_from(&self, resp: SipResponse, src: std::net::SocketAddr) {
        assert!(self.external.is_none(), "a dual-face test names the response's face");
        self.handle_response(resp, src, Face::Internal).await
    }

    /// Where a response arriving on the external face goes (ADR-0048): its next
    /// Via's response address when that is a registry worker, a departed one
    /// included; else that Via's sent-by when the sent-by is one, a worker
    /// whose source was translated on its way to the proxy. `None`: the Via
    /// names no worker, so the response is not relayed. The proxy reflects
    /// nothing to a host a sender names.
    fn external_response_target(&self, next: &Via) -> Option<ProxyAddr> {
        let (host, port) = next.response_target();
        let target = ProxyAddr::new(host, port);
        if self.registry.lookup_by_address(&target).is_some() {
            return Some(target);
        }
        let (host, port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(host, port);
        self.registry.lookup_by_address(&sent_by).is_some().then_some(sent_by)
    }

    /// The stickiness cookie for reverse-path failover toward `worker`: the
    /// bottommost of the proxy's own Record-Route entries (either face) whose
    /// cookie names `worker` as primary (why the bottommost: ADR-0009 X1), else
    /// the cookie on the proxy's own Via, `ours` — the one a response carrying
    /// no Record-Route brings back (`core/request/via_cookie`).
    fn own_cookie_for(
        &self,
        resp: &SipResponse,
        ours: &Via,
        worker: &str,
    ) -> Option<crate::strategy::RouteParams> {
        let names_worker = |params: &crate::strategy::RouteParams| {
            self.strategy.stickiness_primary(params) == Some(worker)
        };
        resp.list::<sip_message::header::RecordRouteEntry>()
            .unwrap_or_default()
            .iter()
            .rev()
            .filter(|entry| {
                let (host, port) = entry.uri().host_port();
                self.is_self_addr(host, port)
            })
            .map(|entry| crate::headers::cookie_params(entry.uri()))
            .find(names_worker)
            .or_else(|| Some(crate::headers::via_cookie_params(ours)).filter(names_worker))
    }
}

#[cfg(test)]
mod reverse_failover_tests;

#[cfg(test)]
mod relay_tests;

#[cfg(test)]
mod via_cookie_tests;
