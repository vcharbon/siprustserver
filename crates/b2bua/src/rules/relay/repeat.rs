//! The responses a transaction layer hands the TU again: a 2xx to an INVITE
//! is repeated end-to-end until ACKed (RFC 3261 §13.3.1.4) and a reliable
//! provisional until PRACKed (RFC 3262 §3), so both reach the rules more than
//! once. These predicates are the one reading of "already handled" the rule
//! that re-ACKs, the executor that absorbs, and the message ring share.

use call::{Call, Dialog};
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
    let taken = dialog.ext.ack_branch.is_some() || dialog.ext.awaited_ack_cseq.is_some();
    if !taken || !(200..300).contains(&resp.status()) || resp.cseq().method() != Method::Invite {
        return false;
    }
    if resp.to().tag().unwrap_or_default() != dialog.sip.remote_tag {
        return false;
    }
    let cseq = resp.cseq().seq();
    super::acked_invite_cseq(dialog) == Some(cseq)
        && call::helpers::find_pending_request(dialog, i64::from(cseq)).is_none()
}

/// Whether `resp`, arriving on `leg_id`, is a copy of a reliable provisional
/// the call already dealt with — relayed under a shown number, or PRACKed by
/// this stack itself — so it is the responder's §3 retransmission the UAC
/// discards outright (RFC 3262 §4). A provisional carrying no `RSeq`, or one
/// answering anything but an INVITE, is never a repeat by this reading.
pub(crate) fn repeated_reliable_provisional(call: &Call, leg_id: &str, resp: &SipResponse) -> bool {
    let cseq = resp.cseq();
    // A CSeq naming no method reads as the INVITE's, as the relay does.
    let answers_invite = cseq.method() == Method::Invite || cseq.method().as_str().is_empty();
    if !(101..200).contains(&resp.status()) || !answers_invite {
        return false;
    }
    let Some(rseq) = super::reliable_rseq(resp) else {
        return false;
    };
    let to_tag = resp.to().tag().unwrap_or_default();
    let cseq_num = i64::from(cseq.seq());
    call::helpers::reliable_provisional_relayed(call, leg_id, to_tag, cseq_num, rseq)
        || call::helpers::pracked_provisional(call, leg_id, to_tag, cseq_num, rseq)
}
