//! The request path's per-call trace seam (ADR-0026): where the proxy takes its
//! sampling decision, and where a forwarded request's facts are recorded.
//!
//! The proxy samples independently of the workers — no span context arrives on
//! the wire and none leaves on it — so the decision is made here, once, from the
//! datagram itself.

use std::net::SocketAddr;

use observe::CallIdentity;
use sip_message::sniff;
use sip_message::trace_sample::TraceSample;
use sip_message::SipRequest;

use crate::addr::ProxyAddr;
use crate::observability::metrics::RoutingDecisionKind;
use crate::trace::{emit, ProxyTraces};

use super::super::ProxyCore;

impl ProxyCore {
    /// Run the admission chain for an initial INVITE and, on success, open the
    /// proxy's root span for the call and record the INVITE that opened it.
    /// A refusal is counted inside the chain and never logged.
    pub(super) fn activate_trace(&self, req: &SipRequest, src: SocketAddr, at_ms: i64) {
        let call_id = req.call_id().as_str();
        let id = CallIdentity {
            call_id,
            from_tag: req.from().tag().unwrap_or(""),
            // The proxy is transaction-less: no To tag exists on the INVITE it
            // routes, and it never mints one.
            to_tag: "",
        };
        if !self.traces.activate(call_id, id, intake_rate(&self.traces, req.image()), at_ms) {
            return;
        }
        emit::sip_in(&self.traces, call_id, at_ms, src, req.image());
    }

    /// Record a forwarded request on the call's span: the datagram that left,
    /// and the routing facts that chose the hop it left for.
    pub(super) fn trace_forward(
        &self,
        call_id: &str,
        decision: RoutingDecisionKind,
        target: &ProxyAddr,
        stickiness: Option<&'static str>,
        wire: &[u8],
    ) {
        emit::forwarded(
            &self.traces,
            call_id,
            self.now_ms() as i64,
            emit::RouteFacts {
                decision,
                target,
                // A DNS-named target's face is only known once resolved.
                face: target.to_socket_addr().map(|sa| self.egress_face(sa)),
                stickiness,
            },
            wire,
        );
    }
}

/// The rate this INVITE's draw runs at: the `X-Trace-Sample` override when the
/// process both honors the header and exports at all, else `None` (the
/// configured rate). With the deployment gate shut the header is not looked up
/// at all — a serving edge cannot be steered into tracing by a caller, and a
/// process with no collector does not scan a datagram no rate can change the
/// fate of.
fn intake_rate(traces: &ProxyTraces, raw: &[u8]) -> Option<f64> {
    if !traces.honors_header() || !traces.exporter_configured() {
        return None;
    }
    match sniff::trace_sample_rate(raw) {
        TraceSample::Rate(rate) => Some(rate),
        TraceSample::Malformed => {
            observe::counters::bump(&observe::counters::TRACE_HEADER_MALFORMED);
            None
        }
        TraceSample::Absent => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use observe::{RateDraw, SampleAdmission, TokenBucket};

    fn traces(honors_header: bool) -> ProxyTraces {
        gate(true, honors_header)
    }

    fn gate(exporter: bool, honors_header: bool) -> ProxyTraces {
        ProxyTraces::new(
            SampleAdmission::new(exporter, 1.0, 10, RateDraw::seeded(2), TokenBucket::default_at(0)),
            honors_header,
        )
    }

    fn invite(rate_header: Option<&str>) -> Vec<u8> {
        let header = rate_header.map(|v| format!("X-Trace-Sample: {v}\r\n")).unwrap_or_default();
        format!(
            "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-trace\r\n\
             From: <sip:alice@example.com>;tag=a1\r\n\
             To: <sip:bob@example.com>\r\n\
             {header}Call-ID: trace@10.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn the_header_rate_reads_only_where_the_process_opted_in() {
        assert_eq!(intake_rate(&traces(true), &invite(Some("0.25"))), Some(0.25));
        assert_eq!(
            intake_rate(&traces(false), &invite(Some("1"))),
            None,
            "a process that did not opt in never reads the caller's rate",
        );
        assert_eq!(intake_rate(&traces(true), &invite(None)), None, "absent = the configured rate");
    }

    #[test]
    fn a_malformed_rate_is_counted_and_falls_back_to_the_configured_one() {
        let before = observe::counters::get(&observe::counters::TRACE_HEADER_MALFORMED);
        assert_eq!(intake_rate(&traces(true), &invite(Some("loads"))), None);
        assert!(observe::counters::get(&observe::counters::TRACE_HEADER_MALFORMED) > before);
    }

    #[test]
    fn an_inert_process_never_scans_the_datagram() {
        assert_eq!(
            intake_rate(&gate(false, true), &invite(Some("1"))),
            None,
            "no collector, no rate can change any outcome",
        );
    }
}
