//! What a UAC owes a final to an INVITE it holds no record of, built from the
//! final alone: the ACK and the BYE of a 2xx (RFC 3261 §13.2.2.4, §15.1.1),
//! and the hop-by-hop ACK of a non-2xx (§17.1.1.3), whose Request-URI the
//! caller supplies from what the request stated on its own Via.

use super::emit;
use super::error::GenerateError;
use super::in_dialog::{route_for_in_dialog, with_routes};
use crate::draft::RequestDraft;
use crate::header::{CSeq, ContentLength, HeaderName, HeaderValue, MaxForwards, Uri, Via};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipRequest, SipResponse};

/// The dialog a 2xx created or answered in, as its UAC reads it from the 2xx
/// alone (RFC 3261 §12.1.2): the Contact is the remote target, the
/// Record-Route reversed the route set. `None` for a 2xx with no Contact.
fn dialog_target(final_response: &SipResponse) -> Option<(Uri, Vec<String>)> {
    let target = final_response.contacts().as_slice().first().map(|c| c.uri().clone())?;
    let route_set: Vec<String> = final_response
        .record_route_set()
        .map(|set| set.reversed().iter().map(HeaderValue::to_wire).collect())
        .unwrap_or_default();
    Some(route_for_in_dialog(target, &route_set))
}

/// A request of the 2xx's dialog: its Request-URI and route from the 2xx, this
/// hop's Via (the 2xx's own on `branch`), From, To and Call-ID echoing it.
fn in_dialog_of(
    final_response: &SipResponse,
    method: Method,
    cseq: u32,
    branch: &str,
) -> Result<RequestDraft, GenerateError> {
    let (uri, routes) =
        dialog_target(final_response).ok_or(GenerateError::Missing("the 2xx's Contact"))?;
    let echoed = |name: HeaderName| final_response.raw_text(name).next().unwrap_or(SipStr::EMPTY);
    let hop: Via = final_response.top_via().clone().with_branch(branch.to_string());
    let draft = RequestDraft::new(method.clone(), uri)
        .push(hop)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(HeaderName::From))
        .push_raw(HeaderName::To, echoed(HeaderName::To))
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId))
        .push(CSeq::new(cseq, method));
    Ok(with_routes(draft, &routes))
}

/// The ACK of a 2xx to an INVITE whose UAC holds neither the INVITE nor the
/// dialog (RFC 3261 §13.2.2.4): its own transaction on `branch`, the 2xx's
/// CSeq number with method ACK, routed by the 2xx's Contact and Record-Route,
/// carrying `answer` as its session description when the 2xx made an offer
/// (RFC 3264 §4).
pub fn generate_ack_for_unheld_2xx(
    final_response: &SipResponse,
    branch: &str,
    answer: Option<Vec<u8>>,
) -> Result<SipRequest, GenerateError> {
    let draft = in_dialog_of(final_response, Method::Ack, final_response.cseq().seq(), branch)?;
    emit::request(emit::framed(draft, answer.unwrap_or_default(), None))
}

/// The BYE ending the dialog of a 2xx whose UAC holds no record of it (RFC
/// 3261 §15.1.1), at `cseq`, on `branch`.
pub fn generate_bye_for_unheld_2xx(
    final_response: &SipResponse,
    cseq: u32,
    branch: &str,
) -> Result<SipRequest, GenerateError> {
    let draft = in_dialog_of(final_response, Method::Bye, cseq, branch)?;
    emit::request(draft.push(ContentLength::new(0)))
}

/// The ACK of a non-2xx final to an INVITE whose UAC holds no record of it
/// (RFC 3261 §17.1.1.3): the INVITE's `request_uri`, the INVITE's top Via as
/// the final echoes it (branch included), less what the next hop stamped on it
/// (`received`, the `rport` value, §18.2.1, RFC 3581 §4), From, To and
/// Call-ID echoing the final, its CSeq number with method ACK. No Route: the
/// caller states the INVITE's route for the hop. No body.
pub fn generate_ack_for_unheld_non_2xx(
    final_response: &SipResponse,
    request_uri: Uri,
) -> Result<SipRequest, GenerateError> {
    let echoed = |name: HeaderName| final_response.raw_text(name).next().unwrap_or(SipStr::EMPTY);
    let mut via = final_response.top_via().clone().without_param("received");
    if via.rport().is_some() {
        via = via.requesting_rport();
    }
    let draft = RequestDraft::new(Method::Ack, request_uri)
        .push(via)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(HeaderName::From))
        .push_raw(HeaderName::To, echoed(HeaderName::To))
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId))
        .push(CSeq::new(final_response.cseq().seq(), Method::Ack));
    emit::request(draft.push(ContentLength::new(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::{SipMessage, SipParser};

    fn response(text: &str) -> SipResponse {
        match CustomParser::new().parse(text.as_bytes()).expect("a response") {
            SipMessage::Response(r) => r,
            SipMessage::Request(_) => panic!("a response"),
        }
    }

    fn two_hundred() -> SipResponse {
        response(
            "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKinv;cr=x\r\n\
Record-Route: <sip:10.0.0.5:5080;lr;e=1>\r\n\
Record-Route: <sip:10.0.0.5:5080;lr;outbound>\r\n\
From: <sip:svc@10.0.0.9>;tag=ours\r\n\
To: <sip:bob@10.0.0.2>;tag=bobs\r\n\
Call-ID: b-1-x@10.0.0.9\r\n\
CSeq: 4 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n",
        )
    }

    #[test]
    fn a_2xx_is_acked_on_its_own_transaction_toward_its_contact() {
        let ack = generate_ack_for_unheld_2xx(&two_hundred(), "z9hG4bKack", None).expect("an ACK");
        assert_eq!(ack.method(), &Method::Ack);
        assert_eq!(ack.request_uri().to_string(), "sip:bob@10.0.0.2:5070");
        assert_eq!(ack.cseq().seq(), 4);
        assert_eq!(ack.top_via().branch(), Some("z9hG4bKack"));
        assert_eq!(ack.to().tag(), Some("bobs"));
        assert_eq!(ack.from().tag(), Some("ours"));
        let routes: Vec<String> = ack.raw_text(HeaderName::Route).map(|r| r.to_string()).collect();
        assert_eq!(routes, ["<sip:10.0.0.5:5080;lr;outbound>", "<sip:10.0.0.5:5080;lr;e=1>"]);
        assert!(ack.body().is_empty());
    }

    #[test]
    fn an_answer_rides_the_ack() {
        let answer = b"v=0\r\n".to_vec();
        let ack = generate_ack_for_unheld_2xx(&two_hundred(), "z9hG4bKack", Some(answer.clone()))
            .expect("an ACK");
        assert_eq!(ack.body(), answer.as_slice());
    }

    #[test]
    fn the_bye_follows_at_the_next_sequence() {
        let bye = generate_bye_for_unheld_2xx(&two_hundred(), 5, "z9hG4bKbye").expect("a BYE");
        assert_eq!(bye.method(), &Method::Bye);
        assert_eq!(bye.cseq().seq(), 5);
        assert_eq!(bye.request_uri().to_string(), "sip:bob@10.0.0.2:5070");
    }

    #[test]
    fn a_2xx_without_a_contact_draws_nothing() {
        let no_contact = response(
            "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKinv\r\n\
From: <sip:svc@10.0.0.9>;tag=ours\r\n\
To: <sip:bob@10.0.0.2>;tag=bobs\r\n\
Call-ID: c\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        );
        assert!(generate_ack_for_unheld_2xx(&no_contact, "z9hG4bKa", None).is_err());
    }

    #[test]
    fn a_non_2xx_is_acked_on_its_own_branch_at_the_stated_uri() {
        let busy = response(
            "SIP/2.0 487 Request Terminated\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKinv;cr=x;rport=5060;received=10.0.0.8\r\n\
From: <sip:svc@10.0.0.9>;tag=ours\r\n\
To: <sip:bob@10.0.0.2>;tag=bobs\r\n\
Call-ID: b-1-x@10.0.0.9\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n",
        );
        let uri = Uri::parse(&SipStr::from_static("sip:bob@10.0.0.2:5070")).expect("a URI");
        let ack = generate_ack_for_unheld_non_2xx(&busy, uri).expect("an ACK");
        assert_eq!(ack.top_via().branch(), Some("z9hG4bKinv"));
        assert_eq!(ack.top_via().received(), None, "what the next hop stamped is left out");
        assert_eq!(
            ack.top_via().to_wire(),
            "SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKinv;cr=x;rport"
        );
        assert_eq!(ack.cseq().seq(), 1);
        assert_eq!(ack.cseq().method(), Method::Ack);
        assert_eq!(ack.request_uri().to_string(), "sip:bob@10.0.0.2:5070");
    }
}
