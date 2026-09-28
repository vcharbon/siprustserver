//! RFC 7315 §5.6 charging correlation on a call's messages. A call whose
//! decision stated a charging vector
//! ([`call::features::FeatureActivations::stated_charging_vector`]) carries it
//! on every message the stack sends on every leg — requests and responses but
//! `100 Trying` — in place of any relayed or minted copy. A call stating none
//! keeps the originated-leg arm ([`minting_arm`]) and the relay of the
//! originator's vector.
//!
//! The stamp is idempotent: a message already carrying exactly the vector is
//! left as it is, so a message stamped before it is retained for repetition
//! and stamped again on its way out stays the same bytes.

use call::features::ChargingVectorFeature;
use call::Call;
use sip_message::header::{ChargingVector, HeaderName, HeaderValue};
use sip_message::{SipRequest, SipResponse};

use crate::effects::{HandlerResult, OutboundBody, OutboundSipEffect};

/// The charging vector a decision stated for `call`, if any.
pub fn stated(call: &Call) -> Option<&str> {
    call.features.as_ref()?.stated_charging_vector.as_deref()
}

/// The arm a leg this call originates mints its own vector under: none where
/// the call carries a stated one, which every message then states instead.
pub fn minting_arm(call: &Call) -> Option<&ChargingVectorFeature> {
    let features = call.features.as_ref()?;
    match features.stated_charging_vector {
        Some(_) => None,
        None => features.charging_vector.as_ref(),
    }
}

/// Stamp every outbound message of `result` with its call's stated vector.
pub fn stamp_outbound(mut result: HandlerResult) -> HandlerResult {
    if let Some(vector) = stated(&result.call).map(str::to_string) {
        for effect in &mut result.effects.outbound {
            stamp_with(&vector, effect);
        }
    }
    result
}

/// Stamp `effect` with `call`'s stated vector, if it states one.
pub fn stamp(call: &Call, effect: &mut OutboundSipEffect) {
    if let Some(vector) = stated(call) {
        stamp_with(vector, effect);
    }
}

/// Stamp `resp` with `call`'s stated vector, if it states one.
pub fn stamp_response(call: &Call, resp: &mut SipResponse) {
    if let Some(stamped) = stated(call).and_then(|v| stamped_response(v, resp)) {
        *resp = stamped;
    }
}

/// A retained datagram is the bytes of a message stamped when it first left.
fn stamp_with(vector: &str, effect: &mut OutboundSipEffect) {
    match &mut effect.body {
        OutboundBody::Request(req) => {
            if let Some(stamped) = stamped_request(vector, req) {
                *req = stamped;
            }
        }
        OutboundBody::Response(resp) => {
            if let Some(stamped) = stamped_response(vector, resp) {
                *resp = stamped;
            }
        }
        OutboundBody::Datagram(_) => {}
    }
}

fn name() -> HeaderName {
    ChargingVector::header_name()
}

/// Whether `lines` are exactly the one line `vector`.
fn carries(mut lines: impl Iterator<Item = impl AsRef<str>>, vector: &str) -> bool {
    lines.next().is_some_and(|l| l.as_ref() == vector) && lines.next().is_none()
}

/// `req` stating `vector` as its one charging line, or `None` where it already
/// does. The line replaces every relayed or minted copy.
fn stamped_request(vector: &str, req: &SipRequest) -> Option<SipRequest> {
    if carries(req.raw(name()), vector) {
        return None;
    }
    req.thaw().remove(&name()).push_raw(name(), vector.to_string()).freeze().ok()
}

/// `resp` stating `vector`, or `None` where it already does or is a `100`,
/// which is hop-by-hop (RFC 3261 §21.1.1) and states none.
fn stamped_response(vector: &str, resp: &SipResponse) -> Option<SipResponse> {
    if resp.status() == 100 || carries(resp.raw(name()), vector) {
        return None;
    }
    resp.thaw().remove(&name()).push_raw(name(), vector.to_string()).freeze().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    const VECTOR: &str = "icid-value=abc;orig-ioi=example.net";

    fn parse(raw: &str) -> SipMessage {
        CustomParser::new().parse(raw.as_bytes()).unwrap()
    }

    fn request(extra: &str) -> SipRequest {
        let raw = format!(
            "BYE sip:bob@192.0.2.9 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.5;branch=z9hG4bK-1\r\n\
Max-Forwards: 70\r\nFrom: <sip:alice@192.0.2.5>;tag=a\r\nTo: <sip:bob@192.0.2.9>;tag=b\r\n\
Call-ID: c1\r\nCSeq: 2 BYE\r\n{extra}Content-Length: 0\r\n\r\n"
        );
        match parse(&raw) {
            SipMessage::Request(r) => r,
            _ => unreachable!(),
        }
    }

    fn response(status: &str) -> SipResponse {
        let raw = format!(
            "SIP/2.0 {status}\r\nVia: SIP/2.0/UDP 192.0.2.5;branch=z9hG4bK-1\r\n\
From: <sip:alice@192.0.2.5>;tag=a\r\nTo: <sip:bob@192.0.2.9>;tag=b\r\n\
Call-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        match parse(&raw) {
            SipMessage::Response(r) => r,
            _ => unreachable!(),
        }
    }

    fn lines(req: &SipRequest) -> Vec<String> {
        req.raw(name()).map(str::to_string).collect()
    }

    /// Every relayed or minted copy gives way to the one stated line.
    #[test]
    fn the_stated_vector_replaces_every_copy() {
        let relayed = request("P-Charging-Vector: icid-value=one\r\np-charging-vector: two\r\n");
        let stamped = stamped_request(VECTOR, &relayed).expect("restated");
        assert_eq!(lines(&stamped), [VECTOR]);
        let bare = stamped_request(VECTOR, &request("")).expect("added");
        assert_eq!(lines(&bare), [VECTOR]);
    }

    /// A message already stating exactly the vector is the same bytes after a
    /// second stamp — what keeps a retained repeat equal to its original.
    #[test]
    fn a_second_stamp_changes_nothing() {
        let once = stamped_request(VECTOR, &request("")).unwrap();
        assert!(stamped_request(VECTOR, &once).is_none());
        let answered = stamped_response(VECTOR, &response("200 OK")).unwrap();
        assert!(stamped_response(VECTOR, &answered).is_none());
    }

    /// `100 Trying` is hop-by-hop and carries none; every other response does.
    #[test]
    fn a_trying_is_left_alone() {
        assert!(stamped_response(VECTOR, &response("100 Trying")).is_none());
        let ringing = stamped_response(VECTOR, &response("180 Ringing")).unwrap();
        assert_eq!(ringing.raw(name()).collect::<Vec<_>>(), [VECTOR]);
    }
}
