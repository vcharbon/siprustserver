//! What the wire stated: a header's value as written, and the finals a party
//! got for its initial INVITE.

use std::net::SocketAddr;

use scenario_harness::RunReport;

/// The first value `req` states under header `name`, as written on the wire.
pub fn stated(req: &sip_message::SipRequest, name: &str) -> Option<String> {
    req.raw_text(sip_message::header::HeaderName::from(name)).next().map(|v| v.as_str().to_string())
}

/// The first value `resp` states under header `name`, as written on the wire.
pub fn stated_by_response(resp: &sip_message::SipResponse, name: &str) -> Option<String> {
    resp.raw_text(sip_message::header::HeaderName::from(name))
        .next()
        .map(|v| v.as_str().to_string())
}

/// The distinct final statuses delivered to `to` for its initial-INVITE
/// transaction (CSeq method INVITE) — the RFC 3261 §17.2.1
/// one-final-per-transaction oracle shared by the cancelled-call scenarios.
/// Retransmits of the SAME final dedup to one status; a
/// regression's second, different final shows up as a second element.
pub fn invite_final_statuses(report: &RunReport, to: SocketAddr) -> Vec<u16> {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{Method, SipMessage, SipParser};
    let mut statuses: Vec<u16> = report
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match CustomParser::built().parse(&e.raw) {
            Ok(SipMessage::Response(r))
                if r.status() >= 200 && *r.cseq().method() == Method::Invite =>
            {
                Some(r.status())
            }
            _ => None,
        })
        .collect();
    statuses.sort_unstable();
    statuses.dedup();
    statuses
}
