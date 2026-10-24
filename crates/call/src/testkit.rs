//! Fixture support: a bare dialled [`Leg`], the facts its derived state
//! reads ([`Leg::state`]), a shown reliable provisional and a re-INVITE round
//! relayed into a dialog, for tests that start a call mid-flight.

use crate::model::{
    CSeq, Dialog, DialogEnd, DialogState, HostPort, Ladder, Leg, LegKind, LegState, OutboundInvite,
    Provisional, ProvisionalId, RSeq, RemoteInfo, RequestRecord, ResponderAck, SentRequest, Shown,
};

/// A reliable provisional `rseq` of the responder `responder_tag` on
/// `leg_id`, answering its INVITE `cseq`, shown as `shown_rseq` in this
/// stack's `shown_tag` dialog under that same CSeq, its ladder at `ladder`,
/// its responder owed `responder`. No description either way.
#[allow(clippy::too_many_arguments)]
pub fn shown_provisional(
    leg_id: &str,
    responder_tag: &str,
    cseq: u32,
    rseq: u32,
    shown_tag: &str,
    shown_rseq: u32,
    ladder: Ladder,
    responder: ResponderAck,
) -> Provisional {
    Provisional {
        id: ProvisionalId {
            leg: leg_id.into(),
            responder_tag: responder_tag.into(),
            invite_cseq: CSeq(cseq),
            rseq: RSeq(rseq),
        },
        responder_sdp: false,
        responder,
        shown: Some(Shown {
            tag: shown_tag.into(),
            rseq: RSeq(shown_rseq),
            invite_cseq: CSeq(cseq),
            carried_sdp: false,
            ladder,
        }),
    }
}

/// Give `leg` the facts that make it read `state`, touching only what that
/// takes. `Early`: an early dialog. `Confirmed`: its first early dialog
/// confirmed, else a confirmed one; a dialled leg's INVITE took a 200.
/// `Terminated`: a confirmed dialog ended by the peer's BYE, else the leg's
/// INVITE took a 487. A dialog it mints is named by the leg's own tag
/// ([`crate::helpers::leg_own_tag`], `"staged"` when it has none) and the
/// peer tag `"staged-peer"`, its target `sip:staged-peer@staged.invalid`.
pub fn stage_leg(leg: &mut Leg, state: LegState) {
    match state {
        LegState::Trying => {}
        LegState::Early => {
            if !leg.dialogs.iter().any(|d| d.state == DialogState::Early) {
                let d = staged_dialog(leg, DialogState::Early);
                leg.dialogs.insert(d);
            }
        }
        LegState::Confirmed => {
            let early = leg.dialogs.iter_mut().find(|d| d.state == DialogState::Early);
            let promoted = early.map(|d| d.state = DialogState::Confirmed).is_some();
            if !promoted && leg.dialogs.confirmed().is_none() {
                let d = staged_dialog(leg, DialogState::Confirmed);
                leg.dialogs.insert(d);
            }
            if let Some(invite) = leg.outbound_invite_mut() {
                invite.record_final(200);
            }
        }
        LegState::Terminated => {
            if let Some(d) = leg.dialogs.iter_mut().find(|d| d.state == DialogState::Confirmed) {
                d.state = DialogState::Ended(DialogEnd::PeerBye);
                return;
            }
            crate::helpers::retire_early_dialogs(leg);
            match leg.outbound_invite_mut() {
                Some(invite) => invite.record_final(487),
                None => {
                    leg.invite_final_sent_mut().get_or_insert(487);
                }
            }
        }
    }
}

fn staged_dialog(leg: &Leg, state: DialogState) -> Dialog {
    let ours = crate::helpers::leg_own_tag(leg).unwrap_or("staged").to_string();
    let cseq = leg.dialled_cseq().unwrap_or(0);
    let mut d = Dialog::new(ours, "staged-peer", state, cseq);
    d.remote_target = "sip:staged-peer@staged.invalid".into();
    d
}

/// A leg this stack dialled as `leg_id` under From-tag `our_tag`, holding no
/// dialog, its INVITE (CSeq 1) unanswered.
pub fn dialled_leg(leg_id: &str, our_tag: &str) -> Leg {
    Leg {
        leg_id: leg_id.to_string(),
        call_id: format!("{leg_id}-call-id"),
        source: RemoteInfo { address: "127.0.0.1".into(), port: 5060 },
        dialogs: Default::default(),
        local_uri: None,
        remote_uri: None,
        invite_request_uri: None,
        invite: Some(RequestRecord::dialled(1, None, OutboundInvite::new(our_tag, 0))),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        in_session_timer: None,
        adopted: None,
        attempt: None,
        ring: crate::model::Ring::Silent,
        messages: Default::default(),
        sdp_session: Default::default(),
    }
}

/// Record what `leg`'s dialling INVITE left as, `sent`: the image a CANCEL,
/// an ACK or a restored record rebuilds from.
pub fn dial_with(leg: &mut Leg, sent: SentRequest) {
    if let Some(record) = leg.invite.as_mut() {
        record.sent = Some(sent);
    }
}

/// The INVITE this stack sent on its own account on a dialog, numbered
/// `cseq`, as it left (`sent`).
pub fn own_invite(cseq: i64, sent: Option<SentRequest>) -> RequestRecord {
    RequestRecord {
        origin: crate::model::RequestOrigin::Own,
        sent,
        ..RequestRecord::relayed("INVITE", cseq, 0)
    }
}

/// Where a staged re-INVITE round stands (RFC 3261 §13.2.2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundStage {
    /// The relay awaits its final.
    Open,
    /// A 2xx was taken; the originator's ACK is awaited.
    Answered,
    /// The originator's ACK left toward the answerer.
    Acked,
    /// A non-2xx ended it: nothing of it is left on the dialog.
    Refused,
}

/// The CSeq the originator of a staged round sent its re-INVITE with.
pub const STAGED_ROUND_CSEQ: i64 = 7;

/// Relay a re-INVITE round into `leg`'s confirmed dialog (staged first when
/// the leg has none) on the client transaction `branch`, and stand it at
/// `stage`. The dialog's own sequence takes the next number.
pub fn stage_reinvite_round(leg: &mut Leg, branch: &str, stage: RoundStage) {
    if leg.dialogs.confirmed().is_none() {
        stage_leg(leg, LegState::Confirmed);
    }
    let Some(d) = leg.dialogs.confirmed_mut() else {
        return;
    };
    d.our_cseq += 1;
    let mut round = RequestRecord {
        sent: Some(SentRequest {
            branch: branch.into(),
            image: Vec::new(),
            destination: HostPort { host: "staged.invalid".into(), port: 5060 },
        }),
        offered: true,
        ..RequestRecord::relayed("INVITE", d.our_cseq, STAGED_ROUND_CSEQ)
    };
    let exchange = round.invite.get_or_insert_with(Default::default);
    match stage {
        RoundStage::Open => {}
        RoundStage::Answered => exchange.awaited_ack_cseq = Some(STAGED_ROUND_CSEQ),
        RoundStage::Acked => exchange.ack_branch = Some(format!("{branch}-ack")),
        RoundStage::Refused => {
            d.sent_invite = None;
            return;
        }
    }
    d.sent_invite = Some(round);
}
