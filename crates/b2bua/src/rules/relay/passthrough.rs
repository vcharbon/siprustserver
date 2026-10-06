//! The §16.6 transparency sets: what a relayed message carries across the
//! back-to-back UA — every header of the source message this stack does not
//! own — plus the RSeq ownership rewrite on relayed reliable provisionals
//! (RFC 3262). The 18x management *policies* that rewrite provisionals live in
//! `rules::relay_first_18x` / `rules::promote_pem`, not here.

use sip_message::generators::{self, RelayDirection, RelayScope};
use sip_message::header::{self, HeaderName, HeaderValue};
use sip_message::{SipHeader as MsgHeader, SipStr};

/// The way a message relayed onto `leg_id` travels: the originator's leg
/// (`"a"`) is the caller's, every other leg is one the B2BUA called.
pub fn toward_leg(leg_id: &str) -> RelayDirection {
    if leg_id == "a" {
        RelayDirection::TowardCaller
    } else {
        RelayDirection::TowardCallee
    }
}

/// What the B2BUA carries transparently from a b-leg response onto the response
/// it mints toward the a-leg (RFC 3261 §16.6): every header it does not own,
/// plus a 3xx / 485's Contact set (its retry targets, §8.1.3.4, §21.4.22),
/// which includes the reliable-provisional negotiation end to end
/// (`Require`/`Supported`, RFC 3262). `RSeq` rides here too, but as a
/// placeholder: it is per-transaction sequencing the face it is shown on
/// owns, so [`own_the_rseq`] restates it before the response leaves.
/// `scope` states the minted response: what it carries where this response
/// had a body (a dropped body leaves every header describing it behind, a
/// replaced one keeps only the header stating the body's role), the
/// privacy-service role and the deployment's relay policy
/// ([`B2buaConfig::relay_scope`](crate::config::B2buaConfig::relay_scope)).
///
/// This is plain transparent relay — distinct from the B2BUA-side 18x
/// management *policies* (`relayFirst18xTo180`/`promote18xPemTo200`), which
/// *rewrite* these provisionals, and from the a-facing 2xx advertisement
/// [`stamp_a_facing_invite_advert`](super::stamp_a_facing_invite_advert) owns.
pub fn relay_response_passthrough_headers(
    resp: &sip_message::SipResponse,
    scope: RelayScope<'_>,
) -> Vec<MsgHeader> {
    let mut carried = generators::relayable_headers(resp.headers(), scope);
    carried.extend(generators::retry_targets(resp.status(), resp.headers()));
    carried
}

/// Restate a relayed reliable provisional's `RSeq` with the number this stack
/// owns. The sender of a reliable provisional owns its sequence, exactly as it
/// owns `CSeq`, so the caller is shown a ladder of this stack's own — one per
/// a-facing early dialog (RFC 3262 §4 as corrected by errata 4603, see
/// [`call::helpers::assign_a_rseq`]) — and the PRACK naming it translates back
/// at [`call::helpers::b_rseq_for`].
pub fn own_the_rseq(headers: &mut [MsgHeader], a_rseq: i64) {
    for h in headers.iter_mut().filter(|h| HeaderName::RSeq.matches(&h.name)) {
        h.value = SipStr::owned(&a_rseq.to_string());
    }
}

/// Relay a reliable provisional UNRELIABLY: drop its `RSeq` and the `100rel`
/// tag from its `Require`, a `Require` left empty going with it. What remains
/// is the ordinary provisional RFC 3262 §3 obliges this stack to send where
/// the originator never offered the extension, or where the request is not an
/// INVITE (the one method the mechanism serves). The responder's own
/// reliability is this stack's to acknowledge, not the originator's.
pub fn strip_reliability(headers: &mut Vec<MsgHeader>) {
    headers.retain(|h| !HeaderName::RSeq.matches(&h.name));
    let mut kept = Vec::with_capacity(headers.len());
    for h in headers.drain(..) {
        if !HeaderName::Require.matches(&h.name) {
            kept.push(h);
            continue;
        }
        let Ok(required) = header::Require::parse(&h.value) else {
            kept.push(h);
            continue;
        };
        let rest = required.without("100rel");
        if !rest.is_empty() {
            kept.push(MsgHeader { name: h.name, value: SipStr::owned(&rest.to_wire()) });
        }
    }
    *headers = kept;
}

#[cfg(test)]
mod response_transparency_tests {
    //! What a b-leg response carries onto the response minted toward the
    //! originator (RFC 3261 §16.6) — the set every relay exit shares.
    use super::*;
    use sip_message::generators::SourceBody;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// A callee 183 carrying the reliable-provisional negotiation, an early-media
    /// authorization, a release cause, its Timestamp echo, a body and the header
    /// describing it, plus the callee's own route set.
    fn b_leg_183() -> sip_message::SipResponse {
        let raw = "SIP/2.0 183 Session Progress\r\n\
Via: SIP/2.0/UDP 10.244.2.7:5080;branch=z9hG4bK-b\r\n\
Record-Route: <sip:proxy.bob.example;lr>\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob-tag\r\n\
Call-ID: b-leg-call-id\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Require: 100rel\r\n\
RSeq: 1\r\n\
Supported: 100rel, timer\r\n\
P-Early-Media: sendrecv\r\n\
Reason: Q.850;cause=17\r\n\
Timestamp: 54\r\n\
Content-Disposition: session;handling=required\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected response"),
        }
    }

    fn names(headers: &[MsgHeader]) -> Vec<String> {
        headers.iter().map(|h| h.name.to_ascii_lowercase()).collect()
    }

    /// The callee's end-to-end headers reach the caller — the RFC 3262
    /// negotiation, the RFC 5009 early-media authorization, the Q.850 cause and
    /// a Timestamp echo of the caller's own request alike — while the callee's
    /// route set and Contact do not.
    #[test]
    fn a_relayed_response_carries_the_callee_end_to_end_set() {
        let carried = names(&relay_response_passthrough_headers(
            &b_leg_183(),
            RelayScope::response_carrying(SourceBody::Verbatim).stamped(Some("7")),
        ));
        for name in ["require", "rseq", "supported", "p-early-media", "reason", "timestamp"] {
            assert!(carried.contains(&name.to_string()), "{name} must ride: {carried:?}");
        }
        for name in ["via", "record-route", "contact", "to", "content-type"] {
            assert!(!carried.contains(&name.to_string()), "{name} must not ride: {carried:?}");
        }
    }

    /// A relayed 3xx carries the peer's redirect targets, every Contact line
    /// in order; the peer's own Contact on a 1xx / 2xx never rides.
    #[test]
    fn a_relayed_redirect_carries_the_peer_contact_set() {
        let raw = "SIP/2.0 302 Moved Temporarily\r\n\
Via: SIP/2.0/UDP 10.244.2.7:5080;branch=z9hG4bK-b\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob-tag\r\n\
Call-ID: b-leg-call-id\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:carol@10.0.0.9>\r\n\
Contact: <sip:dave@10.0.0.10>;q=0.5\r\n\
Content-Length: 0\r\n\r\n";
        let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
            panic!("expected response")
        };
        let carried = relay_response_passthrough_headers(
            &resp,
            RelayScope::response_carrying(SourceBody::Verbatim),
        );
        let contacts: Vec<&str> = carried
            .iter()
            .filter(|h| HeaderName::Contact.matches(&h.name))
            .map(|h| h.value.as_str())
            .collect();
        assert_eq!(contacts, ["<sip:carol@10.0.0.9>", "<sip:dave@10.0.0.10>;q=0.5"]);
    }

    /// A policy that DROPS the body leaves the header describing that body
    /// behind: `handling=required` must not describe a body the caller never
    /// receives.
    #[test]
    fn body_metadata_does_not_outlive_the_body_it_describes() {
        let with_body = names(&relay_response_passthrough_headers(
            &b_leg_183(),
            RelayScope::response_carrying(SourceBody::Verbatim),
        ));
        assert!(with_body.contains(&"content-disposition".to_string()));

        let without = names(&relay_response_passthrough_headers(
            &b_leg_183(),
            RelayScope::response_carrying(SourceBody::Dropped),
        ));
        assert!(!without.contains(&"content-disposition".to_string()), "{without:?}");
        assert!(
            without.contains(&"p-early-media".to_string()),
            "the rest still rides: {without:?}"
        );
    }

    /// A policy that REPLACES the body stages one of the same role, so the
    /// caller still receives a session body and the instruction to reject an
    /// unprocessable one (RFC 3261 §20.11) still describes it truthfully.
    #[test]
    fn a_replaced_body_keeps_the_disposition_that_still_describes_it() {
        let replaced = names(&relay_response_passthrough_headers(
            &b_leg_183(),
            RelayScope::response_carrying(SourceBody::Replaced),
        ));
        assert!(replaced.contains(&"content-disposition".to_string()), "{replaced:?}");
        assert!(
            replaced.contains(&"p-early-media".to_string()),
            "the rest still rides: {replaced:?}"
        );
    }

    /// Stripping reliability leaves an ordinary provisional: no `RSeq`, no
    /// `100rel`, and a `Require` that named nothing else is gone with it —
    /// while every other end-to-end header still rides.
    #[test]
    fn stripping_reliability_leaves_an_ordinary_provisional() {
        let mut headers = relay_response_passthrough_headers(
            &b_leg_183(),
            RelayScope::response_carrying(SourceBody::Verbatim),
        );
        strip_reliability(&mut headers);
        let carried = names(&headers);
        assert!(!carried.contains(&"rseq".to_string()), "{carried:?}");
        assert!(
            !carried.contains(&"require".to_string()),
            "an emptied Require does not ride: {carried:?}"
        );
        for name in ["supported", "p-early-media", "reason"] {
            assert!(carried.contains(&name.to_string()), "{name} still rides: {carried:?}");
        }
    }

    /// A `Require` that also names another extension keeps naming it: only the
    /// `100rel` tag is this stack's to withdraw.
    #[test]
    fn stripping_reliability_keeps_the_other_required_extensions() {
        let mut headers = vec![
            MsgHeader {
                name: SipStr::from_static("Require"),
                value: SipStr::from_static("100rel, timer"),
            },
            MsgHeader { name: SipStr::from_static("RSeq"), value: SipStr::from_static("4711") },
        ];
        strip_reliability(&mut headers);
        assert_eq!(headers.len(), 1, "{headers:?}");
        assert!(HeaderName::Require.matches(&headers[0].name));
        assert_eq!(headers[0].value.as_str(), "timer");
    }
}

#[cfg(test)]
mod session_timer_transparency_tests {
    //! RFC 4028 through the back-to-back UA: the endpoints negotiate the
    //! session interval and refresh it end to end, so every message carrying
    //! that negotiation crosses with it.
    use super::*;
    use sip_message::generators::SourceBody;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    fn parse(raw: &str) -> SipMessage {
        CustomParser::new().parse(raw.as_bytes()).unwrap()
    }

    /// A refresh re-INVITE the caller sends as the refresher, demanding the
    /// extension, stamped with its own clock.
    fn refresh_reinvite() -> sip_message::SipRequest {
        let raw = "INVITE sip:b2bua@10.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-refresh\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5>;tag=alice-tag\r\n\
To: <sip:bob@10.0.0.1>;tag=b2bua-tag\r\n\
Call-ID: a-leg-call-id\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:alice@192.0.2.5:5060>\r\n\
Supported: timer\r\n\
Require: timer, 100rel\r\n\
Session-Expires: 1800;refresher=uac\r\n\
Min-SE: 90\r\n\
Timestamp: 54\r\n\
Date: Mon, 05 Oct 2026 10:00:00 GMT\r\n\
Content-Length: 0\r\n\r\n";
        match parse(raw) {
            SipMessage::Request(r) => r,
            _ => panic!("expected a request"),
        }
    }

    /// The callee's answer to a refresh, as the UAS of RFC 4028 §9 states it.
    fn refresh_answer() -> sip_message::SipResponse {
        let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5080;branch=z9hG4bK-b\r\n\
From: <sip:alice@192.0.2.5>;tag=b2bua-b-tag\r\n\
To: <sip:bob@10.0.0.2>;tag=bob-tag\r\n\
Call-ID: b-leg-call-id\r\n\
CSeq: 2 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Require: timer\r\n\
Session-Expires: 1800;refresher=uac\r\n\
Date: Mon, 05 Oct 2026 10:00:01 GMT\r\n\
Content-Length: 0\r\n\r\n";
        match parse(raw) {
            SipMessage::Response(r) => r,
            _ => panic!("expected a response"),
        }
    }

    fn value_of(headers: &[MsgHeader], name: &str) -> Vec<String> {
        headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.to_string())
            .collect()
    }

    /// The refresh crosses as the refresher sent it: the interval, its floor,
    /// the demand that the UAS support the timer, and its `Date`; its
    /// `Timestamp` becomes the relaying clock's. The `100rel` demand is this
    /// stack's to answer per leg and stays.
    #[test]
    fn a_relayed_refresh_carries_the_session_timer_negotiation() {
        let carried = b2bua_sdk::in_dialog_relay::relayed_request_lines(
            &refresh_reinvite(),
            &[],
            RelayScope::request().stamped(Some("60.000")),
        );
        assert_eq!(value_of(&carried, "Session-Expires"), ["1800;refresher=uac"]);
        assert_eq!(value_of(&carried, "Min-SE"), ["90"]);
        assert_eq!(value_of(&carried, "Require"), ["timer"], "only the session-timer demand");
        assert_eq!(value_of(&carried, "Supported"), ["timer"]);
        assert_eq!(value_of(&carried, "Timestamp"), ["60.000"]);
        assert_eq!(value_of(&carried, "Date"), ["Mon, 05 Oct 2026 10:00:00 GMT"]);
    }

    /// The UAS's answer crosses back with the interval it settled and the
    /// `Require: timer` RFC 4028 §9 obliges it to state, plus its own `Date`.
    #[test]
    fn a_relayed_refresh_answer_carries_the_settled_interval() {
        let carried = relay_response_passthrough_headers(
            &refresh_answer(),
            RelayScope::response_carrying(SourceBody::Verbatim),
        );
        assert_eq!(value_of(&carried, "Session-Expires"), ["1800;refresher=uac"]);
        assert_eq!(value_of(&carried, "Require"), ["timer"]);
        assert_eq!(value_of(&carried, "Date"), ["Mon, 05 Oct 2026 10:00:01 GMT"]);
    }
}
