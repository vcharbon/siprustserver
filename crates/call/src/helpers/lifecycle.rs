//! Where a call stands on the two monotone axes of its life, and how two copies
//! of one call compare on them and on the re-INVITE rounds they share.
//! Replication reconciliation is the caller: a version vector says which copy
//! is newer, these say which copy is *further along the same call* (ADR-0031
//! D3).

use crate::model::{Call, CallModelState, Dialog, Leg};

use super::leg::find_leg;

/// The call's position as `(an answer was recorded for the caller, active <
/// terminating < terminated)`. Both components are monotone over one owner's
/// copy: neither ever moves backward. "Answered" is read from the durable
/// record of the final the a-leg's initial INVITE server transaction took, so
/// it stays true once the leg is `Terminated`, and from the answer a CANCEL
/// pre-empted ([`crate::Leg::answer_preempted`]), so it stays true when the
/// transaction layer's 487 overtook a recorded 2xx.
pub fn lifecycle_position(call: &Call) -> (u8, u8) {
    let ending = match call.state {
        CallModelState::Active => 0,
        CallModelState::Terminating => 1,
        CallModelState::Terminated => 2,
    };
    let answered = super::caller_answered(call) || call.a_leg.answer_preempted.is_some();
    (u8::from(answered), ending)
}

/// Does `incoming` sit BEHIND `held` on either axis, or on a re-INVITE round
/// ([`round_answered_over`])? Then it is a branch off a view its owner left
/// behind — a pre-answer teardown of a call the other owner answered, a live
/// body for a call the other owner is ending, a round still open that the
/// other owner saw answered — never a newer version of the same call.
pub fn lifecycle_regresses(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i.0 < h.0 || i.1 < h.1 || round_answered_over(held, incoming)
}

/// Does `incoming` stand behind `held` as a version of the call one owner
/// would keep over the other? The answer axis ranks first: **an unanswered
/// teardown never ends an answered call**, so an answered body is never
/// behind an unanswered one, however far toward its end the unanswered one
/// went, and an unanswered body is always behind an answered one. Between two
/// bodies alike on the answer, the one less far toward its end is behind;
/// between two alike on both, the one holding open a re-INVITE round the
/// other holds answered ([`round_answered_over`]). The replication gates read
/// it both ways: a reverse flush behind the primary's live copy never folds
/// as the call's newer version, and a forward flush behind the backup's
/// Element is a branch the Element refuses (ADR-0031 D3).
pub fn lifecycle_behind(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    i < h || (i == h && round_answered_over(held, incoming))
}

/// Does `incoming` carry progress `held` lacks — every step `held` made on BOTH
/// axes and on every round they share, and at least one more? Progress is a
/// chain, not a rank: a terminal body whose caller was never answered does not
/// outrank a live answered one.
pub fn lifecycle_advances(incoming: &Call, held: &Call) -> bool {
    let (i, h) = (lifecycle_position(incoming), lifecycle_position(held));
    !lifecycle_regresses(incoming, held) && (i != h || round_answered_over(incoming, held))
}

/// Does `incoming` hold answered a re-INVITE round `held` holds open? A round
/// is the INVITE client transaction one dialog relays toward its peer, named
/// by the leg, the dialog's two tags and the transaction's branch. It is open
/// while the relay awaits its final, and answered once a 2xx was taken there:
/// the ACK awaited or sent (RFC 3261 §13.2.2.4). A round goes open → answered
/// → ACKed, never back, and only the node holding its client transaction sees
/// its final, so the answer is progress a record holding the round open
/// cannot make itself.
pub fn round_answered_over(incoming: &Call, held: &Call) -> bool {
    legs(held).any(|held_leg| {
        let Some(incoming_leg) = find_leg(incoming, &held_leg.leg_id) else {
            return false;
        };
        held_leg.dialogs.iter().any(|held_dialog| {
            open_round(held_dialog).is_some_and(|(branch, inbound_cseq)| {
                incoming_leg
                    .dialogs
                    .by_tags(&held_dialog.our_tag, &held_dialog.peer_tag)
                    .is_some_and(|d| round_answered(d, branch, inbound_cseq))
            })
        })
    })
}

/// The caller's leg and every callee leg.
fn legs(call: &Call) -> impl Iterator<Item = &Leg> {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter())
}

/// The re-INVITE round `dialog` holds open: the branch of its INVITE client
/// transaction and the CSeq its originator sent the INVITE with, while the
/// relay that transaction carries awaits a final.
fn open_round(dialog: &Dialog) -> Option<(&str, i64)> {
    let txn = dialog.pending_invite_txn.as_ref()?;
    let relay =
        dialog.inbound_pending_requests.iter().find(|p| p.method.eq_ignore_ascii_case("INVITE"))?;
    Some((txn.branch.as_str(), relay.inbound_cseq))
}

/// Whether `dialog` holds the round on `branch` answered: no relay awaits its
/// final any more, and a 2xx was taken there — its originator's ACK, carrying
/// `inbound_cseq`, awaited, or the round's ACK sent.
fn round_answered(dialog: &Dialog, branch: &str, inbound_cseq: i64) -> bool {
    let same_round = dialog.pending_invite_txn.as_ref().is_some_and(|t| t.branch == branch);
    let relay_open =
        dialog.inbound_pending_requests.iter().any(|p| p.method.eq_ignore_ascii_case("INVITE"));
    let ack_awaited = dialog.awaited_ack_cseq == Some(inbound_cseq);
    same_round && !relay_open && (ack_awaited || dialog.ack_branch.is_some())
}
