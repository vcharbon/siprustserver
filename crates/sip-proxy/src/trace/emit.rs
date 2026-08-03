//! The guarded emission vocabulary of a traced call at the proxy (ADR-0026).
//!
//! Every function here runs through [`ProxyTraces::with_span`], so the flag
//! check comes FIRST and the detail string is built only once a root span is
//! known to exist. A proxy tracing nothing pays one relaxed load per datagram;
//! an untraced call on a proxy tracing something else pays one map miss. Neither
//! allocates.
//!
//! Facts carry the timestamp of the fact. Bodies are the raw wire bytes, capped
//! at 16 KiB by `observe` with a `truncated=true` marker.

use std::net::SocketAddr;

use observe::TraceEvent;

use crate::addr::ProxyAddr;
use crate::observability::metrics::{Face, RoutingDecisionKind};

use super::registry::ProxyTraces;

/// A datagram the proxy received for the call, with its raw wire bytes.
pub fn sip_in(traces: &ProxyTraces, call_id: &str, at_ms: i64, src: SocketAddr, wire: &[u8]) {
    traces.with_span(call_id, at_ms, |span| {
        span.record(TraceEvent::new("sip.in", at_ms, &format!("from {src}")).with_body(wire));
    });
}

/// A response the proxy received for the call. A response names no source on
/// this seam — the Via chain in its own bytes does — so the detail is what it
/// answers. Returns whether the call is traced, so the relay path knows before
/// it spends anything carrying the call's key across the message it consumes.
pub fn response_in(
    traces: &ProxyTraces,
    call_id: &str,
    at_ms: i64,
    status: u16,
    method: &str,
    wire: &[u8],
) -> bool {
    traces.with_span(call_id, at_ms, |span| {
        span.record(
            TraceEvent::new("sip.in", at_ms, &format!("{status} for {method}")).with_body(wire),
        );
    })
}

/// A request the proxy forwarded: the datagram it put on the wire, plus the
/// routing facts that chose the hop.
pub fn forwarded(
    traces: &ProxyTraces,
    call_id: &str,
    at_ms: i64,
    facts: RouteFacts<'_>,
    wire: &[u8],
) {
    traces.with_span(call_id, at_ms, |span| {
        span.record(
            TraceEvent::new("sip.out", at_ms, &format!("to {}", facts.target)).with_body(wire),
        );
        span.record(TraceEvent::new("route.decision", at_ms, &facts.render()));
    });
}

/// A response the proxy relayed toward the caller, with its raw wire bytes.
pub fn relayed(traces: &ProxyTraces, call_id: &str, at_ms: i64, next_hop: &ProxyAddr, wire: &[u8]) {
    traces.with_span(call_id, at_ms, |span| {
        span.record(TraceEvent::new("sip.out", at_ms, &format!("to {next_hop}")).with_body(wire));
    });
}

/// The call was refused at the intake gate under self-overload — the routing
/// fact that explains why nothing was forwarded.
pub fn shed(traces: &ProxyTraces, call_id: &str, at_ms: i64, reason: &str) {
    traces.with_span(call_id, at_ms, |span| {
        span.record(TraceEvent::new("route.shed", at_ms, reason));
    });
}

/// Why a request went where it went: the decision the §16 ladder reached, the
/// hop it picked, the egress face it leaves on, and — when a stickiness cookie
/// was consulted — whether it named a worker.
pub struct RouteFacts<'a> {
    /// The ladder's outcome (`select_new`, `decode_forward`, `ack_hop`, …).
    pub decision: RoutingDecisionKind,
    /// The hop the request was forwarded to.
    pub target: &'a ProxyAddr,
    /// The face it egresses on; `None` for a DNS-named target, whose face is
    /// only known once resolved.
    pub face: Option<Face>,
    /// `hit` / `backup` / `miss` when the stickiness cookie was consulted,
    /// `None` when the decision never looked at one.
    pub stickiness: Option<&'static str>,
}

impl RouteFacts<'_> {
    fn render(&self) -> String {
        format!(
            "{} target={} face={} stickiness={}",
            self.decision.as_str(),
            self.target,
            self.face.map(Face::as_str).unwrap_or("unresolved"),
            self.stickiness.unwrap_or("n/a"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use observe::{RateDraw, SampleAdmission, TokenBucket};

    const WIRE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\r\n";

    fn traces(exporter: bool) -> ProxyTraces {
        ProxyTraces::new(
            SampleAdmission::new(exporter, 1.0, 10, RateDraw::seeded(5), TokenBucket::default_at(0)),
            false,
        )
    }

    fn identity() -> observe::CallIdentity<'static> {
        observe::CallIdentity { call_id: "c@h", from_tag: "ft", to_tag: "" }
    }

    fn facts(target: &ProxyAddr) -> RouteFacts<'_> {
        RouteFacts {
            decision: RoutingDecisionKind::DecodeForward,
            target,
            face: Some(Face::Internal),
            stickiness: Some("hit"),
        }
    }

    #[test]
    fn a_traced_call_records_its_datagrams_and_routing_facts() {
        let (_guard, log) = observe::test_buffer();
        let traces = traces(true);
        assert!(traces.activate("c@h", identity(), None, 0));
        let target = ProxyAddr::new("10.0.0.2", 5070);

        sip_in(&traces, "c@h", 1, "10.0.0.1:5060".parse().expect("fixture"), WIRE);
        forwarded(&traces, "c@h", 2, facts(&target), WIRE);
        relayed(&traces, "c@h", 3, &target, b"SIP/2.0 200 OK\r\n\r\n");
        shed(&traces, "c@h", 4, "proxy_overload_cps");

        assert!(log.matching("kind=sip.in").iter().any(|e| e.contains("INVITE sip:")));
        assert_eq!(log.matching("kind=sip.out").len(), 2);
        let decision = log.matching("kind=route.decision");
        assert_eq!(decision.len(), 1);
        assert!(decision[0].contains("decode_forward target=10.0.0.2:5070 face=int stickiness=hit"));
        assert!(log.matching("kind=route.shed")[0].contains("proxy_overload_cps"));
    }

    #[test]
    fn an_untraced_proxy_records_nothing() {
        let (_guard, log) = observe::test_buffer();
        let traces = traces(false);
        assert!(!traces.activate("c@h", identity(), None, 0));
        sip_in(&traces, "c@h", 1, "10.0.0.1:5060".parse().expect("fixture"), WIRE);
        assert!(log.lines().is_empty());
    }
}
