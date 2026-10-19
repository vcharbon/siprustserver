//! [`TurnScope`]: what one turn on a call carries from its first gate to its
//! landing, beside the call itself: the identifiers it draws (ADR-0052 §3).

use sip_ids::{IdKey, TurnIds};

/// One turn's own state. Every identifier the turn mints is a draw of
/// [`ids`](Self::ids), so the turn's identifiers are a function of the
/// process's key and the turn's sequence number.
#[derive(Debug)]
pub struct TurnScope {
    /// The turn's identifier source.
    pub ids: TurnIds,
}

impl TurnScope {
    /// The scope of turn `turn_seq` under `key`, nothing drawn yet.
    pub fn new(key: &IdKey, turn_seq: u64) -> Self {
        Self::of(TurnIds::new(key, turn_seq))
    }

    /// The scope of the turn `ids` draws for.
    pub fn of(ids: TurnIds) -> Self {
        Self { ids }
    }
}
