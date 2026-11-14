//! The guard after a rewrite: the summary's numeric fields and the leg's
//! Call-ID are never rewritten apart from the bytes, so the rewritten wire
//! must still agree with them, or the run refuses its output; and the parse
//! map the verification tests compare before and after a rewrite.

use super::AnonymizeError;
use crate::identities;
use crate::model::{self, Flows};
use crate::reading::Reading;

/// The summary's numeric fields (CSeq seq) cannot be rewritten; verify the
/// rewritten wire still agrees with them, refusing divergence loudly.
pub(super) fn guard_numeric_fields(flows: &Flows) -> Result<(), AnonymizeError> {
    for (li, leg) in flows.legs.iter().enumerate() {
        for (mi, msg) in leg.msgs.iter().enumerate() {
            let raw = model::wire(msg);
            if identities::cseq_seq(&raw) != Some(msg.summary.cseq().seq) {
                return Err(AnonymizeError::NumericFieldDiverged {
                    leg: li,
                    msg: mi,
                    field: "cseq",
                });
            }
            if identities::call_id(&raw).as_deref() != Some(leg.call_id.as_str()) {
                return Err(AnonymizeError::NumericFieldDiverged {
                    leg: li,
                    msg: mi,
                    field: "call_id",
                });
            }
        }
    }
    Ok(())
}

/// Which datagrams parse under `reading` (the smoke oracle: anonymization
/// must not create new parse failures — compared before/after by the
/// verification tests).
pub fn parse_ok_map(flows: &Flows, reading: &Reading) -> Vec<Vec<bool>> {
    flows
        .legs
        .iter()
        .map(|leg| leg.msgs.iter().map(|m| identities::parses(&model::wire(m), reading)).collect())
        .collect()
}
