//! Trace intake: the two doors through which a call becomes traced, and the
//! facts that are backfilled when it does (ADR-0026 §3).
//!
//! Door one is the **draw**, run once at the initial INVITE before the decision
//! round trip, so a sampled call's story starts at its own INVITE. Door two is
//! the **engine force-enable**, `trace: true` on a `RouteDecision` — reachable
//! from `/call/new` and from any later `/call/failure` route, which is why it
//! lives here rather than inside the INVITE handler.
//!
//! Both doors end at the same backfill: the INVITE the call arrived on and the
//! auto-100 sip-txn already sent are recorded at THEIR timestamps. Nothing is
//! buffered for an unsampled call — the backfill reads facts the call already
//! carries.

use std::net::SocketAddr;

use call::Call;
use sip_message::SipRequest;

use crate::decision::{CallDecisionError, NewCallResponse};

use super::{emit, registry, sampled};

/// Run the trace admission chain for a brand-new call and stamp the correlation
/// triple on it. `invite_wire` is the datagram the INVITE arrived as, recorded
/// verbatim when the draw admits. The `X-Trace-Sample` rate override is looked
/// up ONLY when this process honors the header AND exports at all — with no
/// collector no rate can change any outcome, so the header is not even read.
pub(crate) fn activate(call: &mut Call, a_invite: &SipRequest, invite_wire: &[u8], now_ms: i64) {
    let traces = registry::traces();
    let rate = intake_rate(&traces, a_invite);
    let Some(ids) = traces.activate(&call.call_ref, registry::call_identity(call), rate, now_ms)
    else {
        return;
    };
    stamp(call, ids);
    backfill(call, invite_wire);
}

/// Honor a decision-engine `trace: true`. Nothing ever un-samples a call, so
/// this is a no-op for one already sampled; a newly activated call is
/// **backfilled** from `invite_wire`. Returns whether THIS call opened the
/// trace, so a caller can also backfill what only it holds (the decision
/// request body it had no reason to serialize while the call was unsampled).
pub(crate) fn force_enable(call: &mut Call, invite_wire: &[u8], now_ms: i64) -> bool {
    if sampled(call) {
        return false;
    }
    let traces = registry::traces();
    let Some(ids) =
        traces.activate(&call.call_ref, registry::call_identity(call), Some(1.0), now_ms)
    else {
        return false;
    };
    stamp(call, ids);
    backfill(call, invite_wire);
    true
}

/// The rate this INVITE's draw runs at: the `X-Trace-Sample` override when the
/// process both honors the header and exports at all, else `None` (the
/// configured rate). With the deployment gate shut the header is not looked up —
/// a serving edge cannot be steered into tracing by a caller.
pub(crate) fn intake_rate(traces: &registry::CallTraces, a_invite: &SipRequest) -> Option<f64> {
    (traces.honors_header() && traces.exporter_configured())
        .then(|| header_rate(a_invite))
        .flatten()
}

/// The JSON body of an outbound decision request, for a traced call only —
/// `None` leaves an unsampled call paying nothing to serialize a copy of a
/// payload nobody will read.
pub(crate) fn request_body<T: serde::Serialize>(call: &Call, request: &T) -> Option<Vec<u8>> {
    sampled(call).then(|| json_body(request))
}

/// Record the `/call/new` round trip as a child span, with the times the
/// request actually left and the response actually landed. `request_json` is
/// `None` exactly when the call is not traced.
pub(crate) fn record_new_call(
    call: &Call,
    request_json: Option<Vec<u8>>,
    response: &Result<NewCallResponse, CallDecisionError>,
    sent_at_ms: i64,
    received_at_ms: i64,
) {
    let Some(request) = request_json else {
        return;
    };
    let (outcome, body) = match response {
        Ok(treatment) => (treatment_name(treatment), json_body(treatment)),
        Err(err) => ("error", err.to_string().into_bytes()),
    };
    emit::round_trip(call, "/call/new", sent_at_ms, &request, received_at_ms, outcome, &body);
}

fn stamp(call: &mut Call, ids: registry::TraceIds) {
    call.trace_id = Some(ids.trace_id);
    call.root_span_id = Some(ids.root_span_id);
    call.sampled = Some(true);
}

/// The `X-Trace-Sample` rate this INVITE asks for. A value that does not read is
/// ignored in favour of the configured rate and counted — a rig that mistyped
/// its rate must not look like one that asked for nothing.
fn header_rate(a_invite: &SipRequest) -> Option<f64> {
    match sip_message::trace_sample::trace_sample(a_invite) {
        sip_message::trace_sample::TraceSample::Rate(rate) => Some(rate),
        sip_message::trace_sample::TraceSample::Malformed => {
            observe::counters::bump(&observe::counters::TRACE_HEADER_MALFORMED);
            None
        }
        sip_message::trace_sample::TraceSample::Absent => None,
    }
}

/// The two facts that precede any decision: the INVITE as it arrived, raw, and
/// the auto-100 sip-txn sent for it (ADR-0022) — whose bytes this layer never
/// held, so it is recorded as a note rather than a wire body.
fn backfill(call: &Call, invite_wire: &[u8]) {
    let src: SocketAddr = format!("{}:{}", call.a_leg.source.address, call.a_leg.source.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    emit::sip_in(call, call.created_at, src, invite_wire);
    emit::sip_out_note(call, call.created_at, "100 Trying (transaction layer)");
}

/// One decision treatment's name, for the round-trip event's outcome.
fn treatment_name(treatment: &NewCallResponse) -> &'static str {
    match treatment {
        NewCallResponse::Route(_) => "route",
        NewCallResponse::Redirect(_) => "redirect",
        NewCallResponse::Reject(_) => "reject",
        NewCallResponse::Relay { .. } => "relay",
    }
}

/// Serialize a decision payload for a trace body. Program-constructed values, so
/// failure is unreachable; an empty body beats losing the event.
pub(crate) fn json_body<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! Pins the `X-Trace-Sample` deployment gate (ADR-0026): the header steers
    //! the draw ONLY in a process that opted in AND exports, a value that does
    //! not read is ignored *and counted*, and neither ever reaches the draw in a
    //! process with no collector.

    use super::intake_rate;
    use crate::trace::CallTraces;
    use observe::{RateDraw, SampleAdmission, TokenBucket};
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser, SipRequest};

    fn traces(exporter: bool, honors_header: bool) -> CallTraces {
        CallTraces::new(
            SampleAdmission::new(
                exporter,
                1e-4,
                200,
                RateDraw::seeded(3),
                TokenBucket::default_at(0),
            ),
            honors_header,
        )
    }

    fn invite(header: Option<&str>) -> SipRequest {
        let line = header.map(|v| format!("X-Trace-Sample: {v}\r\n")).unwrap_or_default();
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-trace-iih\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: trace-iih@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             {line}\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    #[test]
    fn an_opted_in_process_honors_the_header_rate() {
        assert_eq!(intake_rate(&traces(true, true), &invite(Some("1"))), Some(1.0));
        assert_eq!(intake_rate(&traces(true, true), &invite(Some("0.25"))), Some(0.25));
    }

    #[test]
    fn a_process_that_did_not_opt_in_never_reads_the_header() {
        assert_eq!(
            intake_rate(&traces(true, false), &invite(Some("1"))),
            None,
            "the caller must not be able to steer a serving edge into tracing"
        );
    }

    #[test]
    fn with_no_collector_the_header_cannot_change_any_outcome() {
        assert_eq!(intake_rate(&traces(false, true), &invite(Some("1"))), None);
    }

    #[test]
    fn an_absent_header_falls_back_to_the_configured_rate() {
        assert_eq!(intake_rate(&traces(true, true), &invite(None)), None);
    }

    #[test]
    fn a_malformed_value_is_ignored_and_counted() {
        let before = observe::counters::get(&observe::counters::TRACE_HEADER_MALFORMED);
        assert_eq!(intake_rate(&traces(true, true), &invite(Some("loads"))), None);
        assert_eq!(
            observe::counters::get(&observe::counters::TRACE_HEADER_MALFORMED),
            before + 1,
            "a mistyped rate must not look like a call that asked for nothing"
        );
    }
}
