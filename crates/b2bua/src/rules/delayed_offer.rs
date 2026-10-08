//! The offer a b-leg's 2xx carried where the INVITE this stack sent on that
//! leg carried none (RFC 3261 §13.2.1): the ACK for that 2xx MUST carry the
//! answer (§13.2.2.4). Noted on the leg when the 2xx confirms it, read when the
//! caller's ACK is relayed onto it. Kept in the leg's ext slot.

use call::Leg;
use serde::{Deserialize, Serialize};

/// The leg ext slot the note lives in.
pub const SLOT: &str = "delayed-offer";

/// The delayed offer a b-leg's 2xx carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    /// The CSeq the ACK owing the answer carries, in the acknowledging peer's
    /// own sequence space.
    ack_cseq: i64,
    /// The offer, as the 2xx carried it.
    offer: String,
}

/// Note on `leg` the `offer` its 2xx carried, owed an answer by the ACK with
/// CSeq `ack_cseq`.
pub fn note(leg: &mut Leg, ack_cseq: i64, offer: &[u8]) {
    let note = Note { ack_cseq, offer: String::from_utf8_lossy(offer).into_owned() };
    let value = serde_json::to_value(note).unwrap_or_default();
    leg.ext.get_or_insert_with(Default::default).insert(SLOT.to_string(), value);
}

/// The offer the ACK with CSeq `ack_cseq` owes `leg` an answer to, if any.
pub fn owed(leg: &Leg, ack_cseq: i64) -> Option<Vec<u8>> {
    let note: Note = serde_json::from_value(leg.ext.as_ref()?.get(SLOT)?.clone()).ok()?;
    (note.ack_cseq == ack_cseq).then(|| note.offer.into_bytes())
}
