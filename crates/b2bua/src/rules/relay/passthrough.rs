//! The §16.6 transparency sets: what a relayed message carries across the
//! back-to-back UA — every header of the source message this stack does not
//! own — plus the RSeq ownership rewrite on relayed reliable provisionals
//! (RFC 3262). The 18x management *policies* that rewrite provisionals live in
//! `rules::relay_first_18x` / `rules::promote_pem`, not here.

use sip_message::generators::{self, RelayScope};
use sip_message::header::{self, HeaderName};
use sip_message::{SipHeader as MsgHeader, SipRequest, SipStr};

/// What the B2BUA carries transparently from a b-leg response onto the response
/// it mints toward the a-leg (RFC 3261 §16.6): every header it does not own,
/// which includes the reliable-provisional negotiation end to end
/// (`Require`/`Supported`, RFC 3262). `RSeq` rides here too, but as a
/// placeholder: it is per-transaction sequencing the a-leg owns, so
/// [`own_the_rseq`] restates it before the response leaves. `keeps_body` states
/// whether the relayed response carries this response's own body — a policy
/// that drops or replaces the body leaves the headers describing it behind.
///
/// This is plain transparent relay — distinct from the B2BUA-side 18x
/// management *policies* (`relayFirst18xTo180`/`promote18xPemTo200`), which
/// *rewrite* these provisionals, and from the a-facing 2xx advertisement
/// [`stamp_a_facing_invite_advert`] owns.
pub fn relay_response_passthrough_headers(
    resp: &sip_message::SipResponse,
    keeps_body: bool,
) -> Vec<MsgHeader> {
    let scope = RelayScope::response();
    let scope = if keeps_body { scope } else { scope.without_source_body() };
    generators::relayable_headers(resp.headers(), scope)
}

/// The `RSeq` a reliable provisional states (RFC 3262: `Require: 100rel` plus a
/// numeric `RSeq`), or `None` when this response is not one.
pub fn reliable_rseq(resp: &sip_message::SipResponse) -> Option<i64> {
    let requires = resp.header::<header::Require>()?.ok()?;
    if !requires.contains("100rel") {
        return None;
    }
    Some(resp.header::<header::RSeq>()?.ok()?.value() as i64)
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

/// What the B2BUA carries transparently when relaying an in-dialog *request*
/// across the back-to-back UA: every header of the received request it does not
/// own (RFC 3261 §16.6). A relayed REFER keeps its `Refer-To`/`Referred-By`
/// (without them it is malformed) and a relayed NOTIFY its
/// `Event`/`Subscription-State` — the generator states those from its own opts
/// ONLY for a B2BUA-originated NOTIFY, which the relay path leaves unset, so
/// nothing duplicates. The generator restates `RAck` per RFC 3262 §7.2, so the
/// received one is withheld rather than copied.
///
/// `target_declared` names the advertisement halves the face this request is
/// relayed toward states for itself. The peer's value for those is NOT copied:
/// it is a relayed value, not an explicit instruction, and copying it would
/// silently revert the declared narrowing on every re-INVITE.
pub fn relay_request_passthrough_headers(
    req: &SipRequest,
    target_declared: &[HeaderName],
) -> Vec<MsgHeader> {
    let mut headers = generators::relayable_headers(req.headers(), RelayScope::request());
    headers.retain(|h| !target_declared.iter().any(|name| name.matches(&h.name)));
    headers
}

#[cfg(test)]
mod response_transparency_tests {
    //! What a b-leg response carries onto the response minted toward the
    //! originator (RFC 3261 §16.6) — the set every relay exit shares.
    use super::*;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    /// A callee 183 carrying the reliable-provisional negotiation, an early-media
    /// authorization, a release cause, a body and the header describing it, plus
    /// the callee's own route set and clock stamp.
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
    /// negotiation, the RFC 5009 early-media authorization and the Q.850 cause
    /// alike — while the callee's route set, Contact and clock stamp do not.
    #[test]
    fn a_relayed_response_carries_the_callee_end_to_end_set() {
        let carried = names(&relay_response_passthrough_headers(&b_leg_183(), true));
        for name in ["require", "rseq", "supported", "p-early-media", "reason"] {
            assert!(carried.contains(&name.to_string()), "{name} must ride: {carried:?}");
        }
        for name in ["via", "record-route", "contact", "to", "content-type", "timestamp"] {
            assert!(!carried.contains(&name.to_string()), "{name} must not ride: {carried:?}");
        }
    }

    /// A policy that drops or replaces the body leaves the header describing
    /// that body behind: `handling=required` must not describe a body the caller
    /// never receives.
    #[test]
    fn body_metadata_does_not_outlive_the_body_it_describes() {
        let with_body = names(&relay_response_passthrough_headers(&b_leg_183(), true));
        assert!(with_body.contains(&"content-disposition".to_string()));

        let without = names(&relay_response_passthrough_headers(&b_leg_183(), false));
        assert!(!without.contains(&"content-disposition".to_string()), "{without:?}");
        assert!(without.contains(&"p-early-media".to_string()), "the rest still rides: {without:?}");
    }
}
