//! The live surfacing of the retransmission family rules.

use super::*;

pub(super) const LIVE: &[Live] = &[
    // Gates on every role, no relay skip: a lane's rung is its own re-send whatever it forwarded
    // first.
    Live::emitter(RuleId::RungByteIdentical, rung_byte_identical),
];

fn rung_byte_identical(f: &rfc_rules::Finding, cid: &str) -> String {
    use rfc_rules::rules::retransmit::Class;
    let Decision::Violated(rfc_rules::Evidence::RungDiverged {
        rung,
        copies,
        divergent,
        class,
        method,
        status,
        rseq,
        branch,
        region,
        offset,
        first_line,
        rung_line,
        ..
    }) = &f.decision
    else {
        return String::new();
    };
    let what = match (status, rseq) {
        (Some(st), Some(rs)) => format!("{st} RSeq {rs} ({method})"),
        (Some(st), None) => format!("{st} ({method})"),
        (None, _) => format!("{method}, CSeq {}", f.cseq),
    };
    let line = |l: &Option<String>| match l {
        Some(l) => format!("`{l}`"),
        None => "<end of message>".to_string(),
    };
    let also = match divergent {
        0 | 1 => String::new(),
        n => format!(", and {} later copies diverge too", n - 1),
    };
    let clause = Class::from_label(class).map_or("RFC 3261 §17", Class::clause);
    format!(
        "Sent copy {rung} of {copies} of the {class} {what} (callId {cid}, branch {branch}) \
         with other bytes than the first: the two diverge in the {region} at byte {offset} \
         — first {}, this copy {}{also} — a retransmission is the same message, byte for \
         byte ({clause})",
        line(first_line),
        line(rung_line),
    )
}
