//! The dialog state of a simulated UA (RFC 3261 §12) and the one judge of the
//! in-dialog requests it receives, shared by the fluent agent, the reactive
//! actor and the scenario interpreter.
//!
//! - `table` — [`DialogTable`]: the dialogs one UA holds, fed from both
//!   directions, and the verdict on each received in-dialog request.
//! - `verdict` — [`DialogVerdict`]: what is wrong with a request, its rule
//!   name, and the wire answer a UA gives it.
//! - `judge` — [`DialogJudge`]: one UA's table and the verdicts it recorded,
//!   gated by the harness or handed to a driver.
//!
//! Two requests on an ended dialog draw no verdict:
//! - a PRACK on an early dialog a non-2xx final ended (RFC 3262 §3);
//! - a NOTIFY of a subscription the UA opened, after a BYE (RFC 5057 §5.2).
//!
//! A verdict fails the run unless a `dialog-…` waiver naming the receiving UA
//! opts it out (`WaiverScope::rule(DIALOG_UNKNOWN, why).on_party("alice")`);
//! one naming no UA covers nothing.

mod judge;
mod table;
mod verdict;

pub use judge::{covers, is_partyless_dialog_waiver, DialogJudge, DialogJudging, DialogOptIns};
pub use table::{BackwardsCseq, DialogState, DialogTable};
pub use verdict::{
    DialogVerdict, VerdictKind, DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP, DIALOG_EARLY_REQUEST,
    DIALOG_ENDED, DIALOG_ENDED_ANSWERED, DIALOG_UNKNOWN,
};

#[cfg(test)]
mod tests;
