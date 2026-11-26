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
//! - `wire_ahead` — [`AheadOfFinal`]: what the run's recording shows of a
//!   request the UA sent reaching the peer before the final that ended its
//!   dialog took effect there.
//! - `departures` — [`Departures`]: the order a load call's datagrams left
//!   for the wire, where no recording shows the peer's arrivals.
//!
//! Two requests on an ended dialog draw no verdict:
//! - a PRACK on an early dialog a non-2xx final ended (RFC 3262 §3);
//! - a NOTIFY of a subscription the UA opened, after a BYE (RFC 5057 §5.2).
//!
//! On an early dialog the UA's own non-2xx final ended, a request of the peer
//! draws a 481 that is a finding only of a rule it broke on the early dialog
//! ([`VerdictKind::Crossed`]), until the caller's own ACK of that final, or a
//! new INVITE of the caller, reached the UA (RFC 3261 §17.2.1): till then it
//! may cross the final.
//!
//! On an early dialog a non-2xx final of either side ended, a 2xx to a
//! request the UA sent is a `dialog-ended-answered` finding only when the
//! request reached the peer after the final took effect there, the UA's own
//! final arriving or the peer sending its own, whenever the 2xx arrives: the
//! peer took one before on the live dialog (§12.3). The UA's messages are
//! taken to reach the peer in the order it sent them, so one it sent before
//! its own final, or before the peer's reached it, was so taken; on a run
//! that declares a loss model, the wire record shows when a copy arrived, and
//! on a load call, the call's departure log when a copy left ahead of the
//! UA's own final.
//!
//! On a run that declares a loss model, on a dialog a final or a BYE ended,
//! whichever side sent it, a request of the peer draws a 481 that is a
//! finding only of a rule it broke on the dialog before that end when the
//! wire record shows it a retransmission whose lost first copy the peer sent
//! before it sent that end itself, or no later than it read the UA's from
//! its socket (§12.2.2, §15.1.2, §17.1.2.2); any other, and any on a run that
//! declares none, is a `dialog-ended` finding.
//!
//! On a run that declares a loss model, a CSeq gone backwards whose request's
//! first copy (that transaction's) the network lost on its way to the UA (a
//! later request overtook it), or a CSeq skipping over requests all so lost,
//! is answered as ever (500, taken) and no finding
//! ([`VerdictKind::LossExplained`], RFC 3261 §12.2.2).
//!
//! A verdict fails the run unless a `dialog-…` waiver naming the receiving UA
//! opts it out (`WaiverScope::rule(DIALOG_UNKNOWN, why).on_party("alice")`);
//! one naming no UA covers nothing.

mod departures;
mod judge;
mod reliable;
mod table;
mod verdict;
mod wire_ahead;
mod wire_crossing;
mod wire_loss;

pub use departures::Departures;
pub use judge::{
    covers, covers_at, is_partyless_dialog_waiver, DialogJudge, DialogJudging, DialogOptIns,
    RecordedVerdict,
};
pub use table::{BackwardsCseq, DialogState, DialogTable};
pub use verdict::{
    DialogVerdict, VerdictKind, DIALOG_CSEQ_BACKWARDS, DIALOG_CSEQ_SKIP, DIALOG_EARLY_REQUEST,
    DIALOG_ENDED, DIALOG_ENDED_ANSWERED, DIALOG_UNKNOWN,
};
pub use wire_ahead::AheadOfFinal;
pub use wire_crossing::{DialogEnd, Ending, SentBeforeEnd};
pub(crate) use wire_loss::send_lost;
pub use wire_loss::{LostOnTheWay, LostRequest};

#[cfg(test)]
mod tests;
