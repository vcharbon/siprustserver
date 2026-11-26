//! The request path's per-call trace seam (ADR-0026): where the proxy takes its
//! sampling decision, and where a forwarded request's facts are recorded.
//!
//! The proxy samples independently of the workers — no span context arrives on
//! the wire and none leaves on it — so the decision is made here, once, from the
//! INVITE itself.

use std::net::SocketAddr;

use observe::CallIdentity;
use sip_message::trace_sample::{trace_sample, TraceSample};
use sip_message::SipRequest;

use crate::addr::ProxyAddr;
use crate::observability::metrics::{Face, RoutingDecisionKind};
use crate::trace::{emit, Activation, ProxyTraces};

use super::super::ProxyCore;

impl ProxyCore {
    /// Run the sampling admission for an initial INVITE and, on success, open
    /// the proxy's root span for the call and record the INVITE that opened it.
    /// A refusal is counted inside the admission and never logged. A call whose
    /// span is already open (an authentication retry, a retransmission whose
    /// memo was evicted) records nothing here, since the per-packet seam of
    /// `handle_request` already recorded its datagram; its CSeq still moves the
    /// span.
    pub(super) fn activate_trace(&self, req: &SipRequest, src: SocketAddr, at_ms: i64) {
        let call_id = req.call_id().as_str();
        let id = CallIdentity { call_id, from_tag: req.from().tag().unwrap_or(""), to_tag: "" };
        let rate = intake_rate(&self.traces, req);
        if self.traces.activate(call_id, id, req.cseq().seq(), rate, at_ms) != Activation::Opened {
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
        face: Face,
        stickiness: Option<&'static str>,
        wire: &[u8],
    ) {
        emit::forwarded(
            &self.traces,
            call_id,
            self.trace_ms(),
            emit::RouteFacts { decision, target, face, stickiness },
            wire,
        );
    }
}

/// The rate this INVITE's draw runs at: the `X-Trace-Sample` override when the
/// process both honors the header and exports, else `None` (the configured
/// rate). A malformed header is counted and reads as `None`. With either gate
/// shut the header is never read, so a caller cannot steer the proxy into
/// tracing.
fn intake_rate(traces: &ProxyTraces, req: &SipRequest) -> Option<f64> {
    if !traces.honors_header() || !traces.exporter_configured() {
        return None;
    }
    match trace_sample(req) {
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

    use observe::{activation_bucket, RateDraw, SampleAdmission};

    fn traces(honors_header: bool) -> ProxyTraces {
        gate(true, honors_header)
    }

    fn gate(exporter: bool, honors_header: bool) -> ProxyTraces {
        ProxyTraces::new(
            SampleAdmission::new(exporter, 1.0, 10, RateDraw::seeded(2), activation_bucket(0)),
            honors_header,
        )
    }

    fn invite(rate_header: Option<&str>) -> SipRequest {
        use sip_message::{CustomParser, SipMessage, SipParser};
        let header = rate_header.map(|v| format!("X-Trace-Sample: {v}\r\n")).unwrap_or_default();
        let raw = format!(
            "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-trace\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=a1\r\n\
             To: <sip:bob@example.com>\r\n\
             {header}Call-ID: trace@10.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()) {
            Ok(SipMessage::Request(req)) => req,
            other => panic!("the fixture INVITE parses: {other:?}"),
        }
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
    fn an_inert_process_never_reads_the_rate() {
        assert_eq!(
            intake_rate(&gate(false, true), &invite(Some("1"))),
            None,
            "no collector, no rate can change any outcome",
        );
    }
}
