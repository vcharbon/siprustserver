//! CDR (call detail record) writing — port of `CdrWriter.ts`. One record is
//! written per call at termination, carrying the accumulated `Call.cdr_events`.
//! The in-memory writer ([`InMemoryCdrWriter`]) lets tests assert exactly one
//! CDR per call; [`BufferedCdrWriter`] is the production drop-on-overload buffer.

mod buffered;
mod memory;

pub use buffered::BufferedCdrWriter;
pub use memory::InMemoryCdrWriter;

use async_trait::async_trait;
use serde::Serialize;

use call::{Call, CdrEvent, LegDisposition, LegState};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrLeg {
    pub call_id: String,
    pub from_tag: String,
    pub state: LegState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrBLeg {
    pub leg_id: String,
    pub call_id: String,
    pub state: LegState,
    pub disposition: LegDisposition,
}

/// One completed call's record.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CdrRecord {
    pub call_ref: String,
    pub created_at: i64,
    pub terminated_at: i64,
    /// `true` when [`build_record`] had to clamp `terminated_at` up to
    /// `created_at` because the raw stamps arrived out of order — a
    /// **cross-node clock-skew** artifact (clock-skew hardening): `created_at` is
    /// minted on the call's ORIGIN node (replicated raw) while `terminated_at` is
    /// read on the DISCHARGING node, so a host NTP step between the two anchors can
    /// make `terminated_at < created_at` and yield a negative/garbage duration
    /// downstream. Serialized so the skew corruption is VISIBLE in the record
    /// rather than silently producing a bad duration; omitted when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clock_skew_clamped: bool,
    pub a_leg: CdrLeg,
    pub b_legs: Vec<CdrBLeg>,
    pub events: Vec<CdrEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_context: Option<String>,
}

/// Build a [`CdrRecord`] from a terminated call.
///
/// **Cross-node stamp provenance (clock-skew hardening).** `created_at` is minted
/// on the call's ORIGIN node at INVITE time and rides the replicated body raw;
/// `terminated_at` is read on whichever node discharges the call (often a
/// different node after a failover). Under a host clock skew the two can arrive
/// out of order, so we clamp `terminated_at` up to `created_at` (a non-negative
/// duration is the invariant every downstream biller assumes) and set
/// `clock_skew_clamped` so the corruption is visible instead of silent.
pub fn build_record(call: &Call, terminated_at: i64) -> CdrRecord {
    let clock_skew_clamped = terminated_at < call.created_at;
    let terminated_at = terminated_at.max(call.created_at);
    CdrRecord {
        call_ref: call.call_ref.clone(),
        created_at: call.created_at,
        terminated_at,
        clock_skew_clamped,
        a_leg: CdrLeg {
            call_id: call.a_leg.call_id.clone(),
            from_tag: call.a_leg.from_tag.clone(),
            state: call.a_leg.state,
        },
        b_legs: call
            .b_legs
            .iter()
            .map(|l| CdrBLeg {
                leg_id: l.leg_id.clone(),
                call_id: l.call_id.clone(),
                state: l.state,
                disposition: l.disposition,
            })
            .collect(),
        events: call.cdr_events.clone(),
        billing_context: call.billing_context.clone(),
    }
}

/// The CDR sink. `write` is non-blocking-fast; `read_all` is the test accessor.
#[async_trait]
pub trait CdrWriter: Send + Sync {
    async fn write(&self, call: &Call, terminated_at: i64);
    async fn read_all(&self) -> Vec<CdrRecord>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::B2buaConfig;
    use crate::initial_invite::build_initial_call;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use std::net::SocketAddr;

    fn a_call(created_at: i64) -> Call {
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-cdr\r\n\
            Max-Forwards: 70\r\n\
            From: <sip:alice@example.com>;tag=alicetag\r\n\
            To: <sip:bob@example.com>\r\n\
            Call-ID: cdr-skew@10.0.0.9\r\n\
            CSeq: 1 INVITE\r\n\
            Contact: <sip:alice@10.0.0.9:5060>\r\n\
            Content-Length: 0\r\n\r\n";
        let req = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected a request"),
        };
        let cfg = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
        build_initial_call(&req, SocketAddr::from(([10, 0, 0, 9], 5060)), &cfg, created_at)
    }

    #[test]
    fn terminated_before_created_is_clamped_and_flagged() {
        // Cross-node skew: created_at (origin node) is AHEAD of terminated_at
        // (discharging node) → a negative raw duration. The record must clamp
        // terminated_at up to created_at (non-negative duration) AND flag it.
        let call = a_call(1_000_000);
        let rec = build_record(&call, 940_000); // 60 s "before" it was created
        assert_eq!(rec.created_at, 1_000_000);
        assert_eq!(rec.terminated_at, 1_000_000, "clamped up to created_at");
        assert!(rec.clock_skew_clamped, "the skew clamp is flagged");
        assert!(rec.terminated_at >= rec.created_at, "duration is non-negative");
        // The flag serialises (visible corruption signal).
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"clock_skew_clamped\":true"), "flag rendered: {json}");
    }

    #[test]
    fn normal_ordering_is_untouched_and_unflagged() {
        let call = a_call(1_000_000);
        let rec = build_record(&call, 1_030_000); // 30 s call, normal order
        assert_eq!(rec.terminated_at, 1_030_000, "in-order stamp untouched");
        assert!(!rec.clock_skew_clamped);
        // Flag omitted from the wire when false (skip_serializing_if).
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("clock_skew_clamped"), "unflagged omits the field: {json}");
    }
}
