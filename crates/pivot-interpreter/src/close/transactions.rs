//! The INVITE transactions a leg's ladder holds open, as the settle floor
//! reads them (`crate::settle::floor`): the non-2xx finals it sent and is
//! owed an ACK for, and the INVITEs it sent and is owed a final for.

use pivot_schema::bundle::RecordedMessage;

use super::view;

/// A non-2xx final this leg SENT to an INVITE it took and the peer has not
/// acknowledged: the server transaction is Completed (RFC 3261 §17.2.1) until
/// the ACK the peer owes on the INVITE's branch (§17.1.1.3) or Timer H, which
/// runs from `sent_at_us`, the final's first emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnackedFinal {
    pub cseq: u32,
    pub status: u16,
    pub sent_at_us: u64,
}

/// The non-2xx INVITE finals one leg sent and is still owed an ACK for, by
/// CSeq, read off its recorded ladder. A 2xx is not here: its ACK is the
/// peer's own request (§13.2.2.4), not the transaction's closer. Nothing else
/// the leg answers is owed an ACK, so a non-INVITE final never appears.
pub fn unacked_finals(messages: &[RecordedMessage]) -> Vec<UnackedFinal> {
    view(messages)
        .sent_finals
        .into_iter()
        .map(|(cseq, (status, sent_at_us))| UnackedFinal { cseq, status, sent_at_us })
        .collect()
}

/// An INVITE this leg SENT that no final has answered: its client transaction
/// is in Calling until a provisional arrives and in Proceeding after (RFC 3261
/// §17.1.1.2). Timer B runs from `sent_at_us`, the INVITE's first emission,
/// and only while it is Calling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvite {
    pub cseq: u32,
    pub sent_at_us: u64,
    pub proceeding: bool,
}

/// The INVITEs one leg sent that are still awaiting their final, by CSeq,
/// read off its recorded ladder. Any final ends the wait, a 2xx included.
pub fn pending_invites(messages: &[RecordedMessage]) -> Vec<PendingInvite> {
    view(messages)
        .pending_invites
        .into_iter()
        .map(|(cseq, (sent_at_us, proceeding))| PendingInvite { cseq, sent_at_us, proceeding })
        .collect()
}
