//! `X-Trace-Sample` extraction over a parsed request (ADR-0026).
//!
//! The header names the per-call trace sampling RATE the sender asks for. It is
//! attacker-controllable, so honoring it is a process-level deployment gate the
//! *caller* owns — this module only reads the value, and is only asked to when
//! that gate is open.
//!
//! A value reads iff it is a float in `0..=1`. "Absent" and "present but
//! unreadable" are kept apart: the first is the ordinary case, the second is
//! counted, because a rig that meant to trace a call and mistyped the rate must
//! not look identical to one that asked for nothing.

use crate::header::HeaderName;
use crate::types::SipRequest;

/// The wire name of the sampling-rate override header.
pub const TRACE_SAMPLE_HEADER: &str = "X-Trace-Sample";

/// What the request says about its sampling rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TraceSample {
    /// No `X-Trace-Sample` header — use the configured rate.
    Absent,
    /// The header is present but its value is not a float in `0..=1`; it is
    /// ignored in favour of the configured rate, and counted.
    Malformed,
    /// The requested rate.
    Rate(f64),
}

/// Read the `X-Trace-Sample` rate override from `req`. The FIRST instance wins:
/// a repeated header states two rates, and letting a later line raise the first
/// would make the override order-dependent.
pub fn trace_sample(req: &SipRequest) -> TraceSample {
    let Some(raw) = req.raw(HeaderName::from(TRACE_SAMPLE_HEADER)).next() else {
        return TraceSample::Absent;
    };
    match raw.trim().parse::<f64>() {
        Ok(rate) if rate.is_finite() && (0.0..=1.0).contains(&rate) => TraceSample::Rate(rate),
        _ => TraceSample::Malformed,
    }
}

#[cfg(test)]
mod tests {
    //! Pins the grammar: a float in `0..=1` reads, everything else is refused
    //! *as malformed* (never silently absent), and the first instance wins.

    use super::{trace_sample, TraceSample, TRACE_SAMPLE_HEADER};
    use crate::parser::custom::CustomParser;
    use crate::parser::SipParser;
    use crate::types::{SipMessage, SipRequest};

    fn parse(headers: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-trace\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: trace@10.0.0.1\r\n\
             CSeq: 1 INVITE\r\n\
             {headers}\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn with(values: &[&str]) -> SipRequest {
        parse(&values.iter().map(|v| format!("{TRACE_SAMPLE_HEADER}: {v}\r\n")).collect::<String>())
    }

    #[test]
    fn a_float_in_range_reads() {
        for (wire, expected) in [("1", 1.0), ("0", 0.0), ("0.25", 0.25), ("  0.5 ", 0.5)] {
            assert_eq!(trace_sample(&with(&[wire])), TraceSample::Rate(expected), "{wire}");
        }
    }

    #[test]
    fn an_absent_header_is_not_a_malformed_one() {
        assert_eq!(trace_sample(&with(&[])), TraceSample::Absent);
    }

    #[test]
    fn a_malformed_or_out_of_range_value_is_refused_and_named() {
        for wire in ["yes", "1.5", "-0.1", "NaN", "inf", "0.5,0.9"] {
            assert_eq!(trace_sample(&with(&[wire])), TraceSample::Malformed, "{wire}");
        }
    }

    #[test]
    fn the_first_instance_wins() {
        // A second line must not be able to raise the rate the first stated.
        assert_eq!(trace_sample(&with(&["0.1", "1"])), TraceSample::Rate(0.1));
    }

    #[test]
    fn the_lookup_is_case_insensitive() {
        assert_eq!(trace_sample(&parse("x-trace-sample: 0.75\r\n")), TraceSample::Rate(0.75));
    }
}
