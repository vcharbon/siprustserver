//! The responses a transaction layer hands the TU again: a 2xx to an INVITE
//! is repeated end-to-end until ACKed (RFC 3261 §13.3.1.4), so it reaches the
//! rules more than once. This predicate is the one reading of "already
//! handled" the rule that re-ACKs and the message ring share; a reliable
//! provisional's repeat is the engine's to drop before the rules.

use call::Dialog;
use sip_message::{Method, SipResponse};

/// Whether `resp` is a copy of an INVITE 2xx this dialog already took
/// (RFC 3261 §13.3.1.4 — the answerer repeats it until ACKed): the dialog
/// holds the taking's mark — its ACK client transaction, or the ACK it still
/// awaits from the peer while a delayed-offer answer is composable by no one
/// else — the response carries the dialog's remote tag (every fork of one
/// INVITE answers on that INVITE's CSeq, §12.1.2, so the CSeq alone would take
/// a losing fork's late 2xx for a repeat) and echoes the CSeq of the INVITE
/// last sent on it while no pending relay of that CSeq is open (a first-time
/// re-INVITE final is claimed by the relay rules). A 2xx answering anything
/// but an INVITE — the 200 to a CANCEL carries the INVITE's number — is never
/// a copy of one.
pub(crate) fn retransmitted_2xx(dialog: &Dialog, resp: &SipResponse) -> bool {
    let taken = dialog.ack_branch.is_some() || dialog.awaited_ack_cseq.is_some();
    if !taken || !(200..300).contains(&resp.status()) || resp.cseq().method() != Method::Invite {
        return false;
    }
    if resp.to().tag().unwrap_or_default() != dialog.peer_tag {
        return false;
    }
    let cseq = resp.cseq().seq();
    super::acked_invite_cseq(dialog) == Some(cseq)
        && call::helpers::find_relay(dialog, i64::from(cseq)).is_none()
}
