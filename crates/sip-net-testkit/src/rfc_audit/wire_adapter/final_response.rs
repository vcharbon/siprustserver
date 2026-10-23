//! The live surfacing of the final-response family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    Live::emitter(RuleId::SingleFinalPerServerTxn, single_final_per_server_txn)
        .roles(&[UaRole::Uas])
        .skip(Skip::RelayLanes),
    Live::emitter(RuleId::No1xxAfterFinal, no_1xx_after_final)
        .roles(&[UaRole::Uas])
        .skip(Skip::RelayLanes),
];

fn single_final_per_server_txn(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::MultipleFinals {
        first_status,
        second_status,
        divergent,
        method,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    // Every divergent status is named once, on this one finding.
    let also = match divergent.as_slice() {
        [_] => String::new(),
        rest => format!(
            " (and later {})",
            rest[1..].iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
        ),
    };
    format!(
        "Sent a second final {second_status} on the {method} server transaction \
         (callId {cid}, branch {branch}) that already answered {first_status}{also} — a \
         server transaction emits exactly one final response; only a same-status \
         retransmission is legal (RFC 3261 §17.2.1 / §13.3.1.4)",
    )
}

fn no_1xx_after_final(f: &rfc_rules::Finding, cid: &str) -> String {
    let Decision::Violated(rfc_rules::Evidence::LateProvisional {
        status,
        completed_by_status,
        branch,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    format!(
        "Sent a new {status} provisional on an INVITE server transaction (callId {cid}, \
         branch {branch}) after its {completed_by_status} final — a completed \
         transaction emits no further provisionals (RFC 3261 §13.3.1.1 / §17.2.1)",
    )
}
