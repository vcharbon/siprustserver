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
//! - `wire_crossing` — [`SentBeforeEnd`]: what the run's recording shows of
//!   a request reaching a dialog a final or a BYE ended.
//! - `wire_loss` — [`LostOnTheWay`]: what the run's recording shows of the
//!   requests the network lost on their way to the UA.
//!
//! Two requests on an ended dialog draw no verdict:
//! - a PRACK on an early dialog a non-2xx final ended (RFC 3262 §3);
//! - a NOTIFY of a subscription the UA opened, after a BYE (RFC 5057 §5.2).
//!
//! On an early dialog the UA's own non-2xx final ended, a 2xx to a request the
//! UA sent draws none, and a request of the peer draws a 481 that is a finding
//! only of a rule it broke on the early dialog ([`VerdictKind::Crossed`]),
//! until the caller's own ACK of that final,
//! or a new INVITE of the caller, reached the UA (RFC 3261 §17.2.1): till then
//! either may cross the final. The peer's messages are taken to reach the UA
//! in the order it sent them.
//!
//! On a dialog a final or a BYE ended, whichever side sent it, a request of
//! the peer draws a 481 that is a finding only of a rule it broke on the
//! dialog before that end when the wire record shows it a retransmission
//! whose lost first copy the peer sent before it sent that end itself, or no
//! later than it read the UA's from its socket (§12.2.2, §15.1.2, §17.1.2.2);
//! any other is a `dialog-ended` finding.
//!
//! On a run that declares a loss model, a CSeq gone backwards whose request's
//! first copy the network lost on its way to the UA (a later request overtook
//! it), or a CSeq skipping over a request so lost, is answered as ever (500,
//! taken) and no finding ([`VerdictKind::LossExplained`], RFC 3261 §12.2.2).
//!
//! A verdict fails the run unless a `dialog-…` waiver naming the receiving UA
//! opts it out (`WaiverScope::rule(DIALOG_UNKNOWN, why).on_party("alice")`);
//! one naming no UA covers nothing.

mod judge;
mod reliable;
mod table;
mod verdict;
mod wire_crossing;
mod wire_loss;

pub use judge::{
    covers, covers_at, is_partyless_dialog_waiver, DialogJudge, DialogJudging, DialogOptIns,
};
pub use table::{BackwardsCseq, DialogState, DialogTable};
pub use verdict::{
    DialogVerdict, VerdictKind, DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP, DIALOG_EARLY_REQUEST,
    DIALOG_ENDED, DIALOG_ENDED_ANSWERED, DIALOG_UNKNOWN,
};
pub use wire_crossing::{DialogEnd, Ending, SentBeforeEnd};
pub use wire_loss::{LostOnTheWay, LostRequest};

#[cfg(test)]
mod tests;
