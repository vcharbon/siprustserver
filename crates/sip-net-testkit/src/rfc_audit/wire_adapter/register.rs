//! The live surfacing of the REGISTER family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::SerialRegister, serial_register),
    Live::emitter(RuleId::RegisterNoRouteSet, register_no_route_set),
];

fn serial_register(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::ConcurrentRegister {
        aor,
        branch,
        pending_branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "{by} sent REGISTER (callId {cid}, branch {branch}) with new Contact while prior \
         REGISTER (branch {pending_branch}) for AOR {aor} still pending — {{Uac}} RFC 3261 \
         §10.2 / RFC3261-MUST-054",
        by = f.emitter,
    )
}

fn register_no_route_set(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::RegisterCarriesRoute { .. }) = &f.decision else {
        return String::new();
    };
    format!(
        "{by} sent REGISTER (callId {cid}) carries Route header — {{Uac}} RFC 3261 §10.2 / \
         RFC3261-MUST-051",
        by = f.emitter,
    )
}
