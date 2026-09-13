//! The decision-log append: the one writer of
//! [`Call::decision_log`](crate::model::Call) and of the ordinal every
//! message-ring entry and CDR event is stamped with.

use crate::model::{Call, DecisionKind, DecisionMark};

/// Record that a decision of `kind` was applied to the call at `at_ms`:
/// bumps `Call::decision_ordinal` and appends the mark carrying it, so the
/// next message or event written reads the new count.
pub fn mark_decision(
    mut call: Call,
    at_ms: i64,
    kind: DecisionKind,
    leg_id: Option<String>,
    label: Option<String>,
) -> Call {
    call.decision_ordinal += 1;
    call.decision_log.push(DecisionMark {
        ordinal: call.decision_ordinal,
        at_ms,
        kind,
        leg_id,
        label,
    });
    call
}
