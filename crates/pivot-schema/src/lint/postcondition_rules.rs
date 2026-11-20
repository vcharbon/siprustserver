//! The settle contract (§10): a case states what the run must have billed, or
//! says in one greppable token why it cannot; its call-records claim is
//! stated once, as a `{ count, checks }`.

use crate::lint::{Index, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    match &index.pivot.postconditions {
        None => report.error(
            "postconditions/cdr-absent-needs-reason",
            "postconditions",
            "the document states no postconditions, so nothing checks what the run billed",
            "state `postconditions.cdr.count`, or `postconditions.cdr.absent: \"<reason>\"`",
        ),
        Some(postconditions) => {
            if !postconditions.records_are_declared() {
                report.error(
                    "postconditions/cdr-absent-needs-reason",
                    "postconditions.cdr",
                    "no CDR expectation, and no reason given for its absence",
                    "state `cdr.count`, `claims.call-records`, or `cdr.absent: \"<reason>\"` so the gap is greppable",
                );
            }
            let records = match postconditions.call_records() {
                Ok(records) => records,
                Err(why) => {
                    report.error(
                        "postconditions/call-records-claim",
                        "postconditions.claims.call-records",
                        why.to_string(),
                        "state the call-records claim once, as `{ count, checks }`",
                    );
                    None
                }
            };
            let inline = records.map(|records| records.checks).unwrap_or_default();
            for check in postconditions.checks.iter().chain(&inline) {
                if !check.value_is_declarable() {
                    report.error(
                        "checks/value-mismatch",
                        "postconditions",
                        format!("check on {:?} states a value its operator does not take, or omits one it does", check.field),
                        "`eq` and `regex` take a value; `exists` and `absent` take none",
                    );
                }
            }
        }
    }
}
