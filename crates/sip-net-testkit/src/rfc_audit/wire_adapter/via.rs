//! The live surfacing of the Via family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::taker(RuleId::RportEcho, rport_echo).advisory().gating_the_sut(),
    Live::emitter(RuleId::ResponseDestination, response_destination).advisory().gating_the_sut(),
];

fn rport_echo(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RportNotEchoed {
        status,
        request_method,
        branch,
        echoed_empty,
        echoed_port,
        source_port,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    if let (Some(echoed), Some(source)) = (echoed_port, source_port) {
        return format!(
            "response {status} to {request_method} (branch {branch}) echoes rport={echoed} for \
             a request that left from port {source} — RFC 3581 §4 requires the source port"
        );
    }
    if *echoed_empty {
        return format!(
            "response {status} to {request_method} (branch {branch}) keeps an empty rport \
             parameter — RFC 3581 §4 requires the server to set it to the source port"
        );
    }
    format!(
        "response {status} to {request_method} (branch {branch}) dropped the rport \
         parameter the request advertised — RFC 3581 §4 requires the server to echo \
         rport=<source-port>"
    )
}

fn response_destination(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ResponseDestinationDiverged {
        status,
        response_dest_method,
        dest_sent_to,
        via_target,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "response {status} to {response_dest_method} sent to {dest_sent_to}, where its top Via \
         names {via_target} — RFC 3261 §18.2.2 / RFC 3581 §4"
    )
}
