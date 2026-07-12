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
        // Dual-face: the proxy stamps its outbound Via with the EGRESS face's
        // advertise, so a response legitimately names either face here.
        if !self.is_self_addr(&top.host, top_port) {
            // Top Via is not us — not our response to relay.
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        // §16.7 / §16.11: 100 Trying is hop-by-hop — it quenched OUR hop's
        // retransmissions and must not be forwarded upstream.
        if resp.status == 100 {
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
        let sent_by = ProxyAddr::new(next.host.clone(), next.port.unwrap_or(5060));
        if let Some(dest) = self.registry.lookup_by_address(&sent_by) {
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

        // Pop the top Via entry (comma-aware) and forward — serialize from the
        // surgered header list directly (no whole-response clone).
        let mut headers = resp.headers.clone();
        remove_first_header_entry(&mut headers, "via");
        let next_hop = ProxyAddr::new(host, port);
        self.send_to(&sip_message::serialize_response_parts(&resp, &headers), &next_hop).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);

        // ── Hop-by-hop ACK for a non-2xx INVITE final (§17.1.1.3) ───────────
        if (300..700).contains(&resp.status) && resp.cseq.method == "INVITE" {
            // The response echoes the request's From (tag included), so this
            // re-builds exactly the key the INVITE was remembered under.
            let key = call_id_cseq_key(&resp.call_id, resp.from.tag.as_deref(), resp.cseq.seq);
            if let Some(found) = self.cancel_lru.lookup(&key) {
                // The synthesized hop ACK travels the SAME hop the INVITE was
                // forwarded on, so its proxy Via carries THAT face's advertise
                // (matching the branch-correlated INVITE Via the downstream
                // UAS saw).
                let ack_adv = self.egress_advertised(&found.target);
                let ack = generate_proxy_ack_for_non_2xx(
                    &resp,
                    (&found.target.host, found.target.port),
                    &found.branch,
                    (&ack_adv.host, ack_adv.port),
                    // §17.1.1.3: reuse the INVITE's Request-URI verbatim (the
                    // remembered forward), so the downstream UAS sees the ACK
                    // under the URI it was INVITEd on — not a user-stripped
                    // `sip:{target}` (newkahneed-033 ask B).
                    Some(&found.invite_ruri),
                );
                self.send_to(&serialize(&SipMessage::Request(ack)), &found.target).await;
                self.metrics.record_ack_synthesized();

                // Mark the transaction "we already ACKed downstream" so the
                // request path absorbs the upstream's OWN ACK for this final
                // (§17.1.1.3 — hop-by-hop) instead of relaying a second ACK
                // to the callee. The upstream's ACK reuses ITS INVITE branch,
                // which is exactly this relayed response's second Via branch
                // (the hop the final is being forwarded to). Short TTL: the
                // upstream ACKs within its final-retransmit window (a re-sent
                // final refreshes the marker).
                let upstream_branch = param_str(next, "branch").unwrap_or("").to_string();
                self.cancel_lru.remember(
                    &crate::cancel_lru::ack_absorb_key(
                        &resp.call_id,
                        resp.from.tag.as_deref(),
                        resp.cseq.seq,
                    ),
                    crate::cancel_lru::CancelEntry {
                        target: found.target.clone(),
                        branch: found.branch.clone(),
                        upstream_branch,
                        invite_ruri: found.invite_ruri.clone(),
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
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
                    // Either face's advertise is "our" Record-Route (dual-face
                    // stamps the two halves with different hosts).
                    if crate::headers::uri_port_u16(parsed.port)
                        .is_some_and(|p| self.is_self_addr(&parsed.host, p))
                    {
                        return Some(parse_uri_params(&entry));
                    }
                }
            }
        }
        None
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
        async fn select_for_new_dialog(&self, _msg: &SipMessage, _opts: SelectOpts) -> Result<ProxyAddr, SelectError> {
            Err(SelectError::NoTarget { reason: "unused".into() })
        }
        async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
            DecodeResult::ForwardBackup { target: ProxyAddr::new(W2_POD, 5060), is_emergency: false }
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
        let core = ProxyCoreBuilder::new(ProxyAddr::new(PROXY_VIP, 5060), Arc::new(BackupStrategy), reg)
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
        let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
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
        core.handle_response(keepalive_200()).await;

        let sent = ep.sent.lock().unwrap();
        assert_eq!(sent.as_slice(), &[format!("{W2_POD}:5060").parse::<SocketAddr>().unwrap()]);
    }

    // Control: an Alive worker keeps its response, delivered to the SNAT'd
    // received/rport return path per §18.2.2/RFC 3581 — identity changes the
    // LOOKUP key, never the send target.
    #[tokio::test]
    async fn response_to_an_alive_worker_keeps_the_received_rport_target() {
        let (core, ep) = core_with(WorkerHealth::Alive);
        core.handle_response(keepalive_200()).await;

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
        let ep = net.bind_udp(BindUdpOpts::new(format!("{PROXY_VIP}:5060").parse().unwrap(), 64)).await.unwrap();
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
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
        let SipMessage::Response(resp) = CustomParser::default().parse(raw.as_bytes()).unwrap() else {
            panic!("expected response")
        };
        let outbound_forwarded_before = metrics.messages_total();
        core.handle_response(resp).await;
        // One inbound record, one outbound DROPPED record — never a forward.
        assert_eq!(metrics.messages_total(), outbound_forwarded_before + 2);
        let txt = metrics.prometheus_text();
        assert!(txt.contains("sip_messages_total{label=\"outbound:dropped\"} 1"));
        assert!(!txt.contains("outbound:forwarded"), "the 100 must not be relayed upstream");
    }

    /// Endpoint double capturing every sent datagram's bytes.
    #[derive(Default)]
    struct ByteCapturingEndpoint {
        sent: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl sip_net::UdpEndpoint for ByteCapturingEndpoint {
        async fn send_to(&self, buf: &[u8], _dst: std::net::SocketAddr) -> Result<(), sip_net::SendError> {
            self.sent.lock().unwrap().push(buf.to_vec());
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

    // RFC 3261 §17.1.1.3 (newkahneed-033 ask B): the hop-by-hop ACK the proxy
    // synthesizes for a relayed non-2xx INVITE final must carry the SAME
    // Request-URI as the INVITE it acknowledges — the old `sip:{target}`
    // fallback stripped the user-part (`sip:+0411…@uas:6001;user=phone` became
    // `sip:uas:6001`), so any downstream R-URI-keyed demux (the loadgen mux's
    // prefix picker) could no longer attribute the ACK to its leg.
    #[tokio::test]
    async fn synthesized_non_2xx_ack_reuses_the_invite_request_uri() {
        let ep = Arc::new(ByteCapturingEndpoint::default());
        let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(ProxyAddr::new(W1, 5060)));
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

        let raw_486 = format!(
            "SIP/2.0 486 Busy Here\r\n\
Via: SIP/2.0/UDP {PROXY_VIP}:5060;branch=z9hG4bKout\r\n\
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
        core.handle_response(resp).await;
        assert_eq!(metrics.ack_synthesized_total(), 1, "the 486 relay must synthesize the hop ACK");

        let sent = ep.sent.lock().unwrap();
        let ack = sent
            .iter()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .find(|s| s.starts_with("ACK "))
            .expect("a synthesized ACK must have been sent");
        let first_line = ack.lines().next().unwrap();
        assert_eq!(
            first_line,
            format!("ACK {invite_ruri} SIP/2.0"),
            "the synthesized ACK must reuse the INVITE's Request-URI verbatim"
        );
    }

    /// Thin `UdpEndpoint` wrapper so the shared `Arc<ByteCapturingEndpoint>` can
    /// be both handed to the builder (boxed) and inspected by the test.
    struct EpHandle(Arc<ByteCapturingEndpoint>);

    #[async_trait::async_trait]
    impl sip_net::UdpEndpoint for EpHandle {
        async fn send_to(&self, buf: &[u8], dst: std::net::SocketAddr) -> Result<(), sip_net::SendError> {
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
