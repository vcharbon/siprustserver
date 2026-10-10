//! Fixture support: a bare dialled [`Leg`], and the facts its derived state
//! reads ([`Leg::state`]), for tests that start a call mid-flight.

use crate::model::{
    Dialog, DialogEnd, DialogState, Leg, LegKind, LegState, OutboundInvite, RemoteInfo,
};

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
            if let Some(invite) = leg.outbound_invite.as_mut() {
                invite.record_final(200);
            }
        }
        LegState::Terminated => {
            if let Some(d) = leg.dialogs.iter_mut().find(|d| d.state == DialogState::Confirmed) {
                d.state = DialogState::Ended(DialogEnd::PeerBye);
                return;
            }
            crate::helpers::retire_early_dialogs(leg);
            match leg.outbound_invite.as_mut() {
                Some(invite) => invite.record_final(487),
                None => {
                    leg.invite_final_sent.get_or_insert(487);
                }
            }
        }
    }
}

fn staged_dialog(leg: &Leg, state: DialogState) -> Dialog {
    let ours = crate::helpers::leg_own_tag(leg).unwrap_or("staged").to_string();
    let cseq = leg.outbound_invite.as_ref().map_or(0, |i| i.cseq);
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
        outbound_invite: Some(OutboundInvite::new(our_tag, 1, None)),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        in_session_timer: None,
        adopted: None,
        invite_final_sent: None,
        answer_preempted: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    }
}
