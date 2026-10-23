//! A leg's dialog-creating INVITE, read off its request record
//! ([`Leg::invite`]): the INVITE this stack dialled and its dialling facts, or
//! the one it received and the final it sent.

use super::invite_exchange::InviteExchange;
use super::leg::Leg;
use super::outbound_invite::OutboundInvite;
use super::request_record::{RequestRecord, SentRequest};

impl Leg {
    /// The record of the INVITE this stack dialled the leg with; `None` on a
    /// leg whose INVITE it received.
    pub fn dialled_invite(&self) -> Option<&RequestRecord> {
        self.invite.as_ref().filter(|r| r.invite.as_ref().is_some_and(|x| x.dialled.is_some()))
    }

    /// The dialling facts of the INVITE this stack dialled the leg with.
    pub fn outbound_invite(&self) -> Option<&OutboundInvite> {
        self.invite.as_ref()?.invite.as_ref()?.dialled.as_ref()
    }

    /// [`Self::outbound_invite`], to update.
    pub fn outbound_invite_mut(&mut self) -> Option<&mut OutboundInvite> {
        self.invite.as_mut()?.invite.as_mut()?.dialled.as_mut()
    }

    /// The CSeq the leg's dialling INVITE went out with.
    pub fn dialled_cseq(&self) -> Option<i64> {
        self.dialled_invite().map(|r| r.outbound_cseq)
    }

    /// The leg's dialling INVITE as it left, where the record keeps it.
    pub fn dialled_sent(&self) -> Option<&SentRequest> {
        self.dialled_invite()?.sent.as_ref()
    }

    /// The record a dialog the leg's dialling INVITE created holds of that
    /// INVITE, its image read off the leg's record; `None` where the leg's
    /// record keeps no image.
    pub fn dialled_on_dialog(&self) -> Option<RequestRecord> {
        let record = self.dialled_invite().filter(|r| r.sent.is_some())?;
        Some(RequestRecord {
            relayed_by: record.relayed_by.clone(),
            ..RequestRecord::dialled_on_dialog(record.outbound_cseq)
        })
    }

    /// The status of the final this stack sent on the dialog-creating INVITE
    /// it received on the leg; `None` while that INVITE is unanswered.
    pub fn invite_final_sent(&self) -> Option<u16> {
        self.received_exchange()?.final_sent
    }

    /// [`Self::invite_final_sent`], to write: the leg's received INVITE is
    /// recorded first where the record holds none.
    pub fn invite_final_sent_mut(&mut self) -> &mut Option<u16> {
        &mut self.received_exchange_mut().final_sent
    }

    /// The 2xx this stack recorded on the leg's received INVITE that the
    /// transaction layer's autonomous 487 pre-empted (RFC 3261 §9.2).
    pub fn answer_preempted(&self) -> Option<u16> {
        self.received_exchange()?.answer_preempted
    }

    /// [`Self::answer_preempted`], to write.
    pub fn answer_preempted_mut(&mut self) -> &mut Option<u16> {
        &mut self.received_exchange_mut().answer_preempted
    }

    fn received_exchange(&self) -> Option<&InviteExchange> {
        self.invite.as_ref()?.invite.as_ref()
    }

    fn received_exchange_mut(&mut self) -> &mut InviteExchange {
        self.invite
            .get_or_insert_with(|| RequestRecord::received_invite(0))
            .invite
            .get_or_insert_with(InviteExchange::default)
    }
}
