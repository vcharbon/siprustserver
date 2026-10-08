//! The relays still open in caller-facing early dialogs the answer retired
//! (RFC 3261 §12.1.2): requests a losing callee fork sent the caller before
//! the answer, each kept under the a-facing tag it was relayed in until the
//! caller's final for it ends its transaction (§8.1.3.3), so that final
//! reaches the fork that sent the request and never a request of the answered
//! dialog carrying the same CSeq. Kept in the a-leg's ext slot.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{Leg, PendingRequest};

/// The a-leg ext slot the retired relays live in.
pub const SLOT: &str = "retired-faces";

/// Retired a-facing tag → the relays still open in that dialog.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Retired {
    #[serde(default)]
    pending: BTreeMap<String, Vec<PendingRequest>>,
}

impl Retired {
    fn of(leg: &Leg) -> Retired {
        leg.ext
            .as_ref()
            .and_then(|ext| ext.get(SLOT))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    fn store(&self, leg: &mut Leg) {
        let ext = leg.ext.get_or_insert_with(Default::default);
        if self.pending.is_empty() {
            ext.remove(SLOT);
        } else {
            ext.insert(SLOT.to_string(), serde_json::to_value(self).unwrap_or_default());
        }
    }
}

/// Keep `pending`, the relays open in the caller-facing dialog `tag` the
/// answer retires, on the a-leg `leg`.
pub fn retire_pending(leg: &mut Leg, tag: &str, pending: Vec<PendingRequest>) {
    if pending.is_empty() {
        return;
    }
    let mut retired = Retired::of(leg);
    retired.pending.entry(tag.to_string()).or_default().extend(pending);
    retired.store(leg);
}

/// The relay open in the retired caller-facing dialog `tag` whose outbound
/// CSeq is `outbound_cseq`.
pub fn retired_pending(leg: &Leg, tag: &str, outbound_cseq: i64) -> Option<PendingRequest> {
    Retired::of(leg)
        .pending
        .remove(tag)
        .and_then(|ps| ps.into_iter().find(|p| p.outbound_cseq == outbound_cseq))
}

/// Close the relay [`retired_pending`] names: its transaction has its final.
pub fn release_retired(leg: &mut Leg, tag: &str, outbound_cseq: i64) {
    let mut retired = Retired::of(leg);
    if let Some(ps) = retired.pending.get_mut(tag) {
        ps.retain(|p| p.outbound_cseq != outbound_cseq);
        if ps.is_empty() {
            retired.pending.remove(tag);
        }
    }
    retired.store(leg);
}
