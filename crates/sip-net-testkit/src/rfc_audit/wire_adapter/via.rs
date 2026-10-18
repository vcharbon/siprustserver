//! The live surfacing of the Via family rules.

use super::*;

pub(super) const LIVE: &[Live] =
    &[Live::taker(RuleId::RportEcho, rport_echo).advisory().skip(Skip::RelayLanes)];

fn rport_echo(f: &rfc_rules::Finding, _cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RportNotEchoed {
        status,
        request_method,
        branch,
        echoed_empty,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
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
