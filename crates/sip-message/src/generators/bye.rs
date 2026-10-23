//! The BYE that ends a dialog a 2xx created when nothing above the
//! transaction holds it (RFC 3261 §15.1.1): built from the INVITE that opened
//! it and the 2xx that confirmed it.

use super::emit;
use super::error::GenerateError;
use super::in_dialog::{route_for_in_dialog, with_routes};
use crate::draft::RequestDraft;
use crate::header::{CSeq, ContentLength, HeaderName, HeaderValue, MaxForwards};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipRequest, SipResponse};

/// Build the BYE of the dialog `final_response`, a 2xx to the initial
/// `original_invite`, created (RFC 3261 §12.1.2, §15.1.1): the 2xx's Contact
/// is the Request-URI and its Record-Route, reversed, the route set; From is
/// the INVITE's, To the 2xx's, the Call-ID the INVITE's, and the CSeq `cseq`,
/// the dialog's next number, which the caller tracks. `branch` names this
/// hop's new transaction.
pub fn generate_bye_for_2xx_from_invite(
    original_invite: &SipRequest,
    final_response: &SipResponse,
    cseq: u32,
    branch: &str,
) -> Result<SipRequest, GenerateError> {
    let route_set: Vec<String> = final_response
        .record_route_set()
        .map(|set| set.reversed().iter().map(HeaderValue::to_wire).collect())
        .unwrap_or_default();
    let target = final_response
        .contacts()
        .as_slice()
        .first()
        .map(|c| c.uri().clone())
        .unwrap_or_else(|| original_invite.request_uri().clone());
    let (uri, routes) = route_for_in_dialog(target, &route_set);
    let echoed = |raw: Option<SipStr>| raw.unwrap_or(SipStr::EMPTY);
    let hop = original_invite.top_via().clone().with_branch(branch.to_string());

    let draft = RequestDraft::new(Method::Bye, uri)
        .push(hop)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(original_invite.raw_text(HeaderName::From).next()))
        .push_raw(HeaderName::To, echoed(final_response.raw_text(HeaderName::To).next()))
        .push_raw(HeaderName::CallId, echoed(original_invite.raw_text(HeaderName::CallId).next()))
        .push(CSeq::new(cseq, Method::Bye));
    emit::request(with_routes(draft, &routes).push(ContentLength::new(0)))
}
