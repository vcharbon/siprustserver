//! What a message must name in ONE endpoint's OWN traffic — the obligations
//! that can only be judged from that party's seat, against the dialog and
//! transaction state it built as it sent and received. ELEVEN of them, nine
//! reading a message the endpoint TOOK and two reading one it SENT against the
//! same state:
//!
//!   - [`ResponseEchoesRequestVia`] (§8.1.3 / §17.1.3) — a response reproduces
//!     the Via stack of the request the taker sent, top branch included.
//!   - [`ResponseCorrelation`] (§8.1.3.3) — a response's CSeq is one of a
//!     request the taker actually sent.
//!   - [`MidDialogTags`] (§12.2.1.1) — an in-dialog message names the taker's
//!     dialog with a tag the taker minted.
//!   - [`PeerUriStable`] (§12.2.1.1) — the peer's From URI stays the one its
//!     dialog-creating INVITE stated.
//!   - [`DialogCallIdStable`] (§12.1) — a dialog keeps the Call-ID it was
//!     created with.
//!   - [`CancelRequestUri`] / [`CancelViaBranch`] (§9.1) — a CANCEL names the
//!     INVITE transaction it cancels, by Request-URI and by branch.
//!   - [`TagConsistency`] (§17.2.1 / §12.1.1) — a UAS keeps one To-tag across
//!     the responses of one server transaction.
//!   - [`NoToTagOnInitialRequest`] (§8.1.1.2) — the FIRST request an endpoint
//!     puts on a Call-ID names no peer, so it states no To-tag.
//!   - [`InDialogToTag`] (§12.2.1.1) — once the endpoint has watched a dialog
//!     form, early or confirmed, the requests it sends on it carry that
//!     dialog's remote tag.
//!   - [`InDialogFromTag`] (§12.2.1.1) — a request the endpoint takes inside a
//!     dialog it confirmed carries that dialog's remote tag in From.
//!
//! **The state is per `(endpoint, Call-ID)` and built from that endpoint's own
//! stream** (`PeerDialogs`): the tags it minted, the requests it sent, the
//! INVITEs it took, the URI its dialog was created with. Every rule here reads
//! that state as it stood BEFORE the message being judged — evidence the taker
//! itself had at the moment it would have had to reject.
//!
//! **A vantage that relays BOTH directions of one dialog judges none of it.**
//! An endpoint that both sent and took an INVITE on a Call-ID is forwarding, not
//! terminating: the "remote" From and tags on what it receives legitimately
//! alternate between the two parties, so its per-endpoint dialog state conflates
//! them and proves nothing. Those occasions stand `Undecidable`.
//!
//! [`DialogCallIdStable`] deliberately does NOT partition on Call-ID — a
//! partition would make a Call-ID CHANGE invisible by opening a second one —
//! [`TagConsistency`] keys on the server transaction rather than the dialog, and
//! [`InDialogToTag`] needs a witness `PeerDialogs` does not carry (which
//! branch established the dialog, which drew a non-2xx), and [`InDialogFromTag`]
//! one per confirmed dialog (its tag pair), so each walks the view under its
//! own key.

mod cancel_target;
mod dialog_id;
mod from_tag;
mod peer_dialogs;
mod to_tag;
mod transaction;

pub use cancel_target::{CancelRequestUri, CancelViaBranch};
pub use dialog_id::{DialogCallIdStable, MidDialogTags, PeerUriStable};
pub use from_tag::{InDialogFromTag, EARLY_DIALOG_WINDOW_US};
pub use to_tag::{InDialogToTag, NoToTagOnInitialRequest};
pub use transaction::{ResponseCorrelation, ResponseEchoesRequestVia, TagConsistency};

crate::registry::rules! {
    /// RFC 3261 §8.1.3 / §17.1.3: a response reproduces its request's Via stack unchanged, top
    /// branch included, so the UAC can match it to the client transaction. Charges the endpoint
    /// that sent the response; read at the endpoint whose transaction it claims.
    ResponseEchoesRequestVia = "response-echoes-request-via";
    /// RFC 3261 §8.1.3.3: a response's `CSeq` echoes one of a request its taker actually sent — a
    /// `(number, method)` no sent request produced answers a phantom. Charges the endpoint that
    /// sent the response.
    ResponseCorrelation = "response-correlation";
    /// RFC 3261 §12.2.1.1: an in-dialog message names the taker's own dialog — a request's To-tag
    /// and a response's From-tag are tags the taker minted. Charges the endpoint that sent it.
    MidDialogTags = "mid-dialog-tags";
    /// RFC 3261 §12.2.1.1: a peer's From URI on its in-dialog requests stays the URI its
    /// dialog-creating INVITE stated. Charges the endpoint that rewrote it.
    PeerUriStable = "peer-uri-stable";
    /// RFC 3261 §12.1: the Call-ID of a dialog is immutable, so every later in-dialog message
    /// carries the one its dialog-creating INVITE established. Charges the endpoint that changed
    /// it.
    DialogCallIdStable = "dialog-call-id-stable";
    /// RFC 3261 §9.1: a CANCEL's Request-URI equals that of the INVITE it cancels. Charges the
    /// CANCEL's sender.
    CancelRequestUri = "cancel-request-uri";
    /// RFC 3261 §9.1: a CANCEL's top Via branch is the branch of an INVITE its taker has open, so
    /// it reaches that server transaction. Charges the CANCEL's sender.
    CancelViaBranch = "cancel-via-branch";
    /// RFC 3261 §17.2.1 / §12.1.1: a UAS that committed a To-tag on a provisional carries that same
    /// tag on the final of the transaction. Charges that UAS.
    TagConsistency = "tag-consistency";
    /// RFC 3261 §8.1.1.2: a request outside any dialog carries no To-tag — there is no peer yet to
    /// name. Charges the sender.
    NoToTagOnInitialRequest = "no-to-tag-on-initial-request";
    /// RFC 3261 §12.2.1.1: a request sent inside a confirmed dialog carries that dialog's remote
    /// tag in To. Charges the sender that omitted it.
    InDialogToTag = "in-dialog-to-tag";
    /// RFC 3261 §12.2.1.1 / §12.1.2: a request an endpoint takes inside a dialog it confirmed
    /// carries that dialog's remote tag in From. Charges the sender.
    InDialogFromTag = "in-dialog-from-tag";
}

#[cfg(test)]
mod tests;
