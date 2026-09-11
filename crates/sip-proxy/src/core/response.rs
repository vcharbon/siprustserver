//! Response path, single-endpoint: validate ≥2 Via and that the top is us;
//! route to the next Via (received/rport precedence); reverse-path failover
//! to the cookie's `w_bak` when the destination worker is confirmed Dead; pop
//! the top Via entry (comma-aware); forward; remember the ACK-relay hop for a
//! non-2xx INVITE final — the final's sender + the INVITE's outbound branch
//! (the ACK itself travels end-to-end — see `core/request`).

use sip_message::header::Via;
use sip_message::types::SipResponse;
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::cancel_lru::call_id_cseq_key;
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

        // §16.7 / §16.11: 100 Trying is hop-by-hop — it quenched OUR hop's
        // retransmissions and must not be forwarded upstream.
        if resp.status() == 100 {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        let next = &hops[1];
        // received / rport take precedence over sent-by (§18.2.2 / §16.7.3).
        let (next_host, next_port) = next.response_target();
        let mut host = next_host.to_string();
        let mut port = next_port;

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
        //
        // The worker is IDENTIFIED by its Via **sent-by** — its advertised
        // registry address, SNAT-immune (the same signal the request path keys
        // worker-outbound classification on). The received/rport-derived
        // `host:port` above stays the SEND target per §18.2.2/RFC 3581, but it
        // must not be the lookup key: behind the keepalived VIP it is the SNAT
        // node IP + an ephemeral port, which matches no registry entry — that
        // made this whole Dead branch unreachable in production, so a dead
        // worker's in-flight responses were blackholed at its stale SNAT
        // address instead of failing over to `w_bak`.
        let (sent_by_host, sent_by_port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(sent_by_host, sent_by_port);
        if let Some(dest) = self.registry.lookup_by_address(&sent_by) {
            if dest.health == WorkerHealth::Dead {
                match self.find_own_record_route_params(&resp) {
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
                            self.metrics
                                .record_message(Direction::Outbound, MessageResult::Dropped);
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
        // — after a failover that is the survivor, not the INVITE's target —
        // and the INVITE's outbound branch, which the sender's server
        // transaction is keyed on. Gating the relay on the memo (not the
        // INVITE entry alone) keeps a takeover worker's 2xx ACK safe when its
        // reset `IdGen` re-mints a branch aliasing the dead primary's INVITE
        // (see `core/request`). Short TTL: the upstream ACKs within its
        // final-retransmit window (a re-sent final refreshes the memo).
        if (300..700).contains(&resp.status()) && cseq.method() == Method::Invite {
            // The response echoes the request's From (tag included), so this
            // re-builds exactly the key the INVITE was remembered under.
            let call_id = resp.call_id();
            let from = resp.from();
            let key = call_id_cseq_key(call_id.as_str(), from.tag(), cseq.seq());
            if let Some(found) = self.cancel_lru.lookup(&key) {
                let upstream_branch = next.branch().unwrap_or_default().to_string();
                self.cancel_lru.remember(
                    &crate::cancel_lru::ack_hop_key(call_id.as_str(), from.tag(), cseq.seq()),
                    crate::cancel_lru::CancelEntry {
                        target: ProxyAddr::from(src),
                        branch: found.branch.clone(),
                        upstream_branch,
                        stickiness: None,
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }
    }

    /// The params of the proxy's own Record-Route entry on a response (echoed
    /// by the UAS per §16.6) — the stickiness cookie for reverse-path failover.
    /// Either face's advertise is "our" Record-Route: dual-face stamps the two
    /// halves with different hosts.
    fn find_own_record_route_params(
        &self,
        resp: &SipResponse,
    ) -> Option<crate::strategy::RouteParams> {
        resp.list::<sip_message::header::RecordRouteEntry>()
            .ok()?
            .iter()
            .find(|entry| {
                let (host, port) = entry.uri().host_port();
                self.is_self_addr(host, port)
            })
            .map(|entry| crate::headers::cookie_params(entry.uri()))
    }
}

#[cfg(test)]
mod reverse_failover_tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};

    use crate::addr::ProxyAddr;
    use crate::core::{ProxyCore, ProxyCoreBuilder};
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::{WorkerEntry, WorkerHealth, WorkerRegistry};
    use crate::strategy::{DecodeResult, RouteParams, RoutingStrategy, SelectError, SelectOpts};

    const W1_POD: &str = "10.244.5.8";
    const W2_POD: &str = "10.244.5.9";
    const UAC: &str = "10.244.7.13";
    const PROXY_VIP: &str = "172.20.255.250";
    const SNAT_NODE: &str = "172.20.0.11";

    /// Endpoint double recording every send's destination.
    #[derive(Default)]
    struct CapturingEndpoint {
        sent: Mutex<Vec<SocketAddr>>,
    }

    #[async_trait]
    impl UdpEndpoint for CapturingEndpoint {
        async fn send_to(&self, _buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
            self.sent.lock().unwrap().push(dst);
            Ok(())
        }
        async fn recv(&self) -> Option<UdpPacket> {
            std::future::pending().await
        }
        fn try_recv(&self) -> Option<UdpPacket> {
            None
        }
        fn local_addr(&self) -> SocketAddr {
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
        }
        fn queue_depth(&self) -> usize {
            0
        }
        fn queue_max(&self) -> usize {
            0
        }
        fn counters(&self) -> UdpEndpointCounters {
            UdpEndpointCounters::default()
        }
    }

    /// Strategy double whose cookie decode fails over to w2.
    struct BackupStrategy;

    #[async_trait]
    impl RoutingStrategy for BackupStrategy {
        fn name(&self) -> &str {
            "Backup"
        }
        async fn select_for_new_dialog(
            &self,
            _msg: &SipMessage,
            _opts: SelectOpts,
        ) -> Result<ProxyAddr, SelectError> {
            Err(SelectError::NoTarget { reason: "unused".into() })
        }
        async fn decode_stickiness(
            &self,
            _params: &RouteParams,
            _msg: &SipMessage,
        ) -> DecodeResult {
            DecodeResult::ForwardBackup {
                target: ProxyAddr::new(W2_POD, 5060),
                is_emergency: false,
            }
        }
        fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
            None
        }
    }

    fn core_with(w1_health: WorkerHealth) -> (ProxyCore, Arc<CapturingEndpoint>) {
        let ep = Arc::new(CapturingEndpoint::default());
        let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![
            WorkerEntry {
                id: "w1".into(),
                address: ProxyAddr::new(W1_POD, 5060),
                health: w1_health,
                draining_since: None,
                first_seen_at_ms: None,
            },
            WorkerEntry::alive("w2", ProxyAddr::new(W2_POD, 5060)),
        ]));
        let core =
            ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), Arc::new(BackupStrategy), reg)
                .clock(Clock::test_at(0))
                .build(Box::new(CapturingEndpointHandle(ep.clone())));
        (core, ep)
    }

    /// Box-able forwarding wrapper (the builder takes `Box<dyn UdpEndpoint>`;
    /// the test keeps the `Arc` to read captures).
    struct CapturingEndpointHandle(Arc<CapturingEndpoint>);

    #[async_trait]
    impl UdpEndpoint for CapturingEndpointHandle {
        async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
            self.0.send_to(buf, dst).await
        }
        async fn recv(&self) -> Option<UdpPacket> {
            self.0.recv().await
        }
        fn try_recv(&self) -> Option<UdpPacket> {
            self.0.try_recv()
        }
        fn local_addr(&self) -> SocketAddr {
            self.0.local_addr()
        }
        fn queue_depth(&self) -> usize {
            self.0.queue_depth()
        }
        fn queue_max(&self) -> usize {
            self.0.queue_max()
        }
        fn counters(&self) -> UdpEndpointCounters {
            self.0.counters()
        }
    }

    /// A keepalive 200 heading back to the worker: top Via = the proxy, next
    /// Via = the worker's sent-by, SNAT'd received/rport stamped by the request
    /// path, the proxy's own cookie Record-Route echoed by the UAS.
    fn keepalive_200() -> sip_message::types::SipResponse {
        let raw = format!(
            "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout;rport\r\n\
Via: SIP/2.0/UDP {W1_POD}:5060;branch=z9hG4bKka;received={SNAT_NODE};rport=63522\r\n\
Record-Route: <sip:{PROXY_VIP}:5060;w_pri=w1;w_bak=w2;lr>\r\n\
From: <sip:service@{PROXY_VIP}:5060>;tag=svc\r\n\
To: <sip:sipp@{UAC}:5060>;tag=uactag\r\n\
Call-ID: ka-1@{UAC}\r\n\
CSeq: 2 OPTIONS\r\n\
Content-Length: 0\r\n\r\n"
        );
        let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap()
        else {
            unreachable!()
        };
        resp
    }

    // Regression: the Dead-worker lookup was keyed on the received/rport-derived
    // address — behind the VIP that is the SNAT node IP + ephemeral port, which
    // matches no registry entry, so the failover branch was unreachable and a
    // dead worker's responses were blackholed at its stale SNAT address. The
    // worker must be identified by its Via SENT-BY (registry identity).
    #[tokio::test]
    async fn response_to_a_dead_worker_fails_over_to_the_backup() {
        let (core, ep) = core_with(WorkerHealth::Dead);
        core.handle_response(keepalive_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
    }

    // Control: an Alive worker keeps its response, delivered to the SNAT'd
    // received/rport return path per §18.2.2/RFC 3581 — identity changes the
    // LOOKUP key, never the send target.
    #[tokio::test]
    async fn response_to_an_alive_worker_keeps_the_received_rport_target() {
        let (core, ep) = core_with(WorkerHealth::Alive);
        core.handle_response(keepalive_200(), format!("{W1_POD}:5060").parse().unwrap()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.as_slice(), &[format!("{SNAT_NODE}:63522").parse::<SocketAddr>().unwrap()]);
    }
}

#[cfg(test)]
mod hop_by_hop_tests {
    use std::sync::Arc;

    use sip_clock::Clock;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_net::types::BindUdpOpts;
    use sip_net::{SignalingNetwork, SimulatedSignalingNetwork};

    use crate::addr::ProxyAddr;
    use crate::core::ProxyCoreBuilder;
    use crate::registry::static_reg::StaticWorkerRegistry;
    use crate::registry::WorkerRegistry;
    use crate::strategies::forward_all::ForwardAllStrategy;
    use crate::{ProxyMetrics, RoutingStrategy};

    const UAC: &str = "10.244.7.13";
    const W1: &str = "10.0.0.1";
    const PROXY_VIP: &str = "172.20.255.250";

    // §16.7 / §16.11: 100 Trying is hop-by-hop — the worker's 100 quenched the
    // proxy→worker hop; relaying it upstream leaks the wrong scope.
    #[tokio::test]
    async fn trying_100_is_absorbed_not_relayed() {
        let net = SimulatedSignalingNetwork::new(1);
        let ep = net
            .bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64))
            .await
            .unwrap();
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
        let metrics = Arc::new(ProxyMetrics::new());
        let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .build(ep);

        let raw = format!(
            "SIP/2.0 100 Trying\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKin\r\n\
From: <sip:alice@{UAC}>;tag=t\r\n\
To: <sip:bob@10.0.0.50>\r\n\
Call-ID: t100-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap()
        else {
            panic!("expected response")
        };
        let outbound_forwarded_before = metrics.messages_total();
        core.handle_response(resp, format!("{W1}:5060").parse().unwrap()).await;
        // One inbound record, one outbound DROPPED record — never a forward.
        assert_eq!(metrics.messages_total(), outbound_forwarded_before + 2);
        let txt = metrics.prometheus_text();
        assert!(txt.contains("sip_messages_total{label=\"outbound:dropped\"} 1"));
        assert!(!txt.contains("outbound:forwarded"), "the 100 must not be relayed upstream");
    }

    /// Endpoint double capturing every sent datagram's destination + bytes.
    #[derive(Default)]
    struct ByteCapturingEndpoint {
        sent: std::sync::Mutex<Vec<(std::net::SocketAddr, Vec<u8>)>>,
    }

    #[async_trait::async_trait]
    impl sip_net::UdpEndpoint for ByteCapturingEndpoint {
        async fn send_to(
            &self,
            buf: &[u8],
            dst: std::net::SocketAddr,
        ) -> Result<(), sip_net::SendError> {
            self.sent.lock().unwrap().push((dst, buf.to_vec()));
            Ok(())
        }
        async fn recv(&self) -> Option<sip_net::UdpPacket> {
            std::future::pending().await
        }
        fn try_recv(&self) -> Option<sip_net::UdpPacket> {
            None
        }
        fn local_addr(&self) -> std::net::SocketAddr {
            format!("{PROXY_VIP}:5060").parse().unwrap()
        }
        fn queue_depth(&self) -> usize {
            0
        }
        fn queue_max(&self) -> usize {
            0
        }
        fn counters(&self) -> sip_net::UdpEndpointCounters {
            sip_net::UdpEndpointCounters::default()
        }
    }

    // RFC 3261 §16.11 / ADR-0022 X4 — loss-recovery of a rejected call. The
    // proxy must NOT synthesize the §17.1.1.3 hop ACK when relaying a non-2xx
    // INVITE final: that quenched the downstream UAS's Timer G (the only
    // retransmitter in the system) while the upstream relay stays
    // exactly-once, so one lost relayed copy wedged the caller until Timer B
    // / the 32 s safety timer. Reliability is end-to-end instead — the
    // upstream's own ACK is relayed downstream to the node the final arrived
    // from, on the INVITE's outbound Via branch (so the UAS's server
    // transaction matches it, §17.2.3), the caller's message otherwise
    // verbatim (R-URI included — the downstream demux keys on it).
    #[tokio::test]
    async fn relayed_non_2xx_final_is_never_hop_acked_and_the_upstream_ack_relays() {
        let ep = Arc::new(ByteCapturingEndpoint::default());
        let strategy: Arc<dyn RoutingStrategy> =
            Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
        let metrics = Arc::new(ProxyMetrics::new());
        let reg: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), strategy, reg)
            .clock(Clock::test_at(0))
            .metrics(metrics.clone())
            .build(Box::new(EpHandle(ep.clone())));

        // A full callee-shaped R-URI: user-part + `;user=phone`, as a worker's
        // b-leg INVITE carries it through the LB.
        let invite_ruri = "sip:+0411133166602012@uas.example:6001;user=phone";
        let raw_invite = format!(
            "INVITE {invite_ruri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+0411133166602012@uas.example>\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        let msg = CustomParser::default().parse(raw_invite.as_bytes()).unwrap();
        core.handle_request(msg, format!("{UAC}:5060").parse().unwrap()).await;

        // The branch the proxy stamped on its forwarded INVITE — the relayed
        // ACK must repeat it exactly.
        let forwarded_invite = {
            let sent = ep.sent.lock().unwrap();
            String::from_utf8_lossy(&sent[0].1).to_string()
        };
        let proxy_branch = forwarded_invite
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("via:"))
            .and_then(|l| l.split("branch=").nth(1))
            .map(|b| b.split(&[';', ',', ' '][..]).next().unwrap().to_string())
            .expect("forwarded INVITE must carry a proxy Via branch");
        ep.sent.lock().unwrap().clear();

        let raw_486 = format!(
            "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch={proxy_branch}\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+0411133166602012@uas.example>;tag=callee-1\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        );
        let SipMessage::Response(resp) = CustomParser::default().parse(raw_486.as_bytes()).unwrap()
        else {
            panic!("expected response")
        };
        core.handle_response(resp, format!("{W1}:5060").parse().unwrap()).await;
        {
            let sent = ep.sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "the 486 relay must be the ONLY send — no synthesized ACK");
            let relayed = String::from_utf8_lossy(&sent[0].1).to_string();
            assert!(
                relayed.starts_with("SIP/2.0 486"),
                "expected the relayed 486, got: {}",
                relayed.lines().next().unwrap_or("")
            );
        }
        ep.sent.lock().unwrap().clear();

        // The upstream's own §17.1.1.3 ACK (same branch as its INVITE) is
        // relayed downstream on the INVITE's hop.
        let raw_ack = format!(
            "ACK {invite_ruri} SIP/2.0\r\n\
Via: SIP/2.0/UDP {UAC}:5060;branch=z9hG4bKackuri;rport\r\n\
Max-Forwards: 70\r\n\
From: <sip:worker@{UAC}>;tag=w1\r\n\
To: <sip:+0411133166602012@uas.example>;tag=callee-1\r\n\
Call-ID: ackuri-1@test\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n"
        );
        let ack_msg = CustomParser::default().parse(raw_ack.as_bytes()).unwrap();
        core.handle_request(ack_msg, format!("{UAC}:5060").parse().unwrap()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "the upstream's ACK must be relayed, not absorbed");
        let (dst, bytes) = &sent[0];
        assert_eq!(
            *dst,
            format!("{W1}:5060").parse::<std::net::SocketAddr>().unwrap(),
            "the ACK goes to the node the final came from (the INVITE's target here)"
        );
        let ack = String::from_utf8_lossy(bytes).to_string();
        assert_eq!(
            ack.lines().next().unwrap(),
            format!("ACK {invite_ruri} SIP/2.0"),
            "the relayed ACK must keep the upstream's Request-URI verbatim"
        );
        let top_via = ack.lines().find(|l| l.to_ascii_lowercase().starts_with("via:")).unwrap();
        assert!(
            top_via.contains(&format!("branch={proxy_branch}")),
            "the relayed ACK must reuse the INVITE's outbound branch (got: {top_via})"
        );
    }

    /// Thin `UdpEndpoint` wrapper so the shared `Arc<ByteCapturingEndpoint>` can
    /// be both handed to the builder (boxed) and inspected by the test.
    struct EpHandle(Arc<ByteCapturingEndpoint>);

    #[async_trait::async_trait]
    impl sip_net::UdpEndpoint for EpHandle {
        async fn send_to(
            &self,
            buf: &[u8],
            dst: std::net::SocketAddr,
        ) -> Result<(), sip_net::SendError> {
            self.0.send_to(buf, dst).await
        }
        async fn recv(&self) -> Option<sip_net::UdpPacket> {
            self.0.recv().await
        }
        fn try_recv(&self) -> Option<sip_net::UdpPacket> {
            self.0.try_recv()
        }
        fn local_addr(&self) -> std::net::SocketAddr {
            self.0.local_addr()
        }
        fn queue_depth(&self) -> usize {
            self.0.queue_depth()
        }
        fn queue_max(&self) -> usize {
            self.0.queue_max()
        }
        fn counters(&self) -> sip_net::UdpEndpointCounters {
            self.0.counters()
        }
    }
}
