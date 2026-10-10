//! Leg-level helpers: role resolution, leg lookup, the dialog of a leg named
//! by the one tag a caller does not already know, state and disposition
//! setters, and termination-resolution predicates.

use crate::model::{
    ByeDisposition, Call, CallModelState, Dialog, DialogEnd, DialogState, EndingBy, Leg,
    LegDisposition, LegKind, LegState, MessageDirection, MessageEntry, RetiredDialog,
};

use super::lens::update_leg;

/// Resolve a leg's role, defaulting from `legId` when `kind` is absent.
pub fn leg_kind(leg: &Leg) -> LegKind {
    leg.kind.unwrap_or(if leg.leg_id == "a" { LegKind::A } else { LegKind::Destination })
}

/// Whether the generic relay / keepalive / failover rules own this leg.
/// `media` and an un-realigned `transfer-target` are unadopted; the explicit
/// `adopted` flag wins.
pub fn is_adopted(leg: &Leg) -> bool {
    if let Some(a) = leg.adopted {
        return a;
    }
    !matches!(leg_kind(leg), LegKind::Media | LegKind::TransferTarget)
}

/// The dialog of a leg this stack dialled whose peer tag is `peer_tag`: its
/// own tag is the From-tag of the leg's INVITE ([`Leg::outbound_invite`]).
pub fn callee_dialog<'a>(leg: &'a Leg, peer_tag: &str) -> Option<&'a Dialog> {
    let ours = &leg.outbound_invite.as_ref()?.our_tag;
    leg.dialogs.by_tags(ours, peer_tag)
}

/// The dialog of `leg` whose peer tag is `peer_tag`, on either face: on a leg
/// this stack dialled, its [`callee_dialog`]; on a leg whose INVITE it
/// received, where every dialog carries the caller's tag, the one the leg's
/// requests ride ([`crate::Dialogs::carrying`]) when it is that peer's.
pub fn dialog_with_peer<'a>(leg: &'a Leg, peer_tag: &str) -> Option<&'a Dialog> {
    if leg.outbound_invite.is_some() {
        return callee_dialog(leg, peer_tag);
    }
    leg.dialogs.carrying().filter(|d| d.peer_tag == peer_tag)
}

/// [`callee_dialog`], to update.
pub fn callee_dialog_mut<'a>(leg: &'a mut Leg, peer_tag: &str) -> Option<&'a mut Dialog> {
    let ours = leg.outbound_invite.as_ref()?.our_tag.clone();
    leg.dialogs.by_tags_mut(&ours, peer_tag)
}

/// This stack's tag on `leg` for a message no dialog names (RFC 3261
/// §8.2.6.2): the From-tag of the INVITE it sent the leg, else the tag of the
/// dialog it answers the leg's INVITE under ([`Leg::own_tag`]).
pub fn leg_own_tag(leg: &Leg) -> Option<&str> {
    leg.outbound_invite.as_ref().map(|i| i.our_tag.as_str()).or(leg.own_tag.as_deref())
}

/// The dialog a request this stack originates on `leg` rides: the leg's
/// confirmed dialog, else — before the leg's INVITE is answered — the dialog
/// it answers that INVITE under ([`Leg::own_tag`]).
pub fn carrying_dialog(leg: &Leg) -> Option<&Dialog> {
    leg.dialogs.confirmed().or_else(|| own_dialog(leg).filter(|d| d.is_live()))
}

/// The dialog `leg` answers its INVITE under ([`Leg::own_tag`]), in any
/// state; `None` on a leg this stack dialled, and before it holds a dialog.
pub fn own_dialog(leg: &Leg) -> Option<&Dialog> {
    let own = leg.own_tag.as_deref()?;
    leg.dialogs.iter().find(|d| d.our_tag == own)
}

/// The caller-facing dialog whose own tag is `our_tag`: its peer tag is the
/// caller's, the From-tag of her INVITE.
pub fn caller_dialog<'a>(call: &'a Call, our_tag: &str) -> Option<&'a Dialog> {
    call.a_leg.dialogs.by_tags(our_tag, &call.a_leg_invite.from_tag)
}

/// [`caller_dialog`], to update.
pub fn caller_dialog_mut<'a>(call: &'a mut Call, our_tag: &str) -> Option<&'a mut Dialog> {
    call.a_leg.dialogs.by_tags_mut(our_tag, &call.a_leg_invite.from_tag)
}

/// Set the state of a specific leg. A leg brought to `Terminated` lets go of
/// the early dialogs its INVITE left open: each retires, with the relays
/// still open in it (RFC 3261 §12.1.2; the INVITE's late final ends nothing
/// more).
pub fn set_leg_state(call: Call, leg_id: &str, state: LegState) -> Call {
    update_leg(call, leg_id, |l| {
        l.state = state;
        if state == LegState::Terminated {
            retire_early_dialogs(l);
        }
    })
}

/// Retire every early dialog of `leg`, its open relays kept for their finals
/// (RFC 3261 §8.1.3.3).
pub fn retire_early_dialogs(leg: &mut Leg) {
    for d in leg.dialogs.iter_mut().filter(|d| d.state == DialogState::Early) {
        let open_relays = std::mem::take(&mut d.inbound_pending_requests);
        d.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
    }
}

/// Set the disposition of a specific leg.
pub fn set_leg_disposition(call: Call, leg_id: &str, disposition: LegDisposition) -> Call {
    update_leg(call, leg_id, |l| l.disposition = disposition)
}

/// Note the CANCEL of `leg_id`'s INVITE leaving (RFC 3261 §9.1): each early
/// dialog that INVITE opened is ending, and ends on the INVITE's final.
pub fn cancel_early_dialogs(call: Call, leg_id: &str) -> Call {
    update_leg(call, leg_id, |l| {
        for d in l.dialogs.iter_mut().filter(|d| d.state == DialogState::Early) {
            d.state = DialogState::Ending(EndingBy::Cancel);
        }
    })
}

/// Note the final or the timeout of the BYE on `bye_branch` that released a
/// retired dialog (RFC 3261 §15.1.1): that release is settled.
pub fn settle_straggler_release(mut call: Call, bye_branch: &str) -> Call {
    let legs = std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut());
    for d in legs.flat_map(|l| l.dialogs.iter_mut()) {
        if let DialogState::Retired(RetiredDialog { release: Some(r), .. }) = &mut d.state {
            if r.bye_branch == bye_branch {
                r.settled = true;
            }
        }
    }
    call
}

/// Set the BYE disposition of a specific leg.
pub fn set_bye_disposition(call: Call, leg_id: &str, bye: ByeDisposition) -> Call {
    update_leg(call, leg_id, |l| l.bye_disposition = Some(bye))
}

/// Record the final sent on a leg's initial inbound INVITE
/// ([`Leg::invite_final_sent`]). The first final stands: a leg that already
/// carries one is left as is — the transaction took exactly that one final.
/// A non-2xx final ends every early dialog of the leg (RFC 3261 §12.1).
pub fn record_invite_final(call: Call, leg_id: &str, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        if l.invite_final_sent.is_none() && status >= 300 {
            reject_early_dialogs(l, status);
        }
        l.invite_final_sent.get_or_insert(status);
    })
}

/// Record the non-2xx final `status` that answered, on `leg_id`, the INVITE
/// of CSeq `cseq` this stack sent: when it is the INVITE that dialled the leg
/// ([`Leg::outbound_invite`]), every early dialog it created ends (RFC 3261
/// §12.1). A final to a re-INVITE changes no dialog's state (§14.1).
pub fn record_dialling_invite_rejected(call: Call, leg_id: &str, cseq: i64, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        if l.outbound_invite.as_ref().is_some_and(|i| i.cseq == cseq) {
            reject_early_dialogs(l, status);
        }
    })
}

/// End every early dialog of `leg` — its CANCEL in flight or not — under
/// the non-2xx final `status` its dialog-creating INVITE took (RFC 3261 §12.1).
pub fn reject_early_dialogs(leg: &mut Leg, status: u16) {
    for d in leg.dialogs.iter_mut().filter(|d| d.awaits_invite_final()) {
        d.state = DialogState::Ended(DialogEnd::Rejected(status));
    }
}

/// Record the `487` the transaction layer answered a CANCEL of `leg_id`'s
/// initial INVITE with (RFC 3261 §9.2). It is the final the wire carries even
/// when the record already holds a 2xx: one built after the layer's 487, which
/// the completed transaction never sent. That 2xx is kept as
/// [`Leg::answer_preempted`], the leg goes back to the early dialog its
/// provisional opened (no confirmation, no 2xx awaiting its ACK), and its
/// message ring drops the 2xx entry, so the ring holds only what reached the
/// wire. Every dialog the INVITE opened, the one the 2xx would have
/// confirmed included, ends under the 487 (RFC 3261 §12.1).
pub fn record_cancelled_invite(call: Call, leg_id: &str) -> Call {
    update_leg(call, leg_id, |l| {
        if let Some(answer) = l.invite_final_sent.filter(|s| (200..300).contains(s)) {
            l.answer_preempted = Some(answer);
            l.messages.entries.retain(|e| !is_sent_invite_2xx(e));
            if l.state == LegState::Confirmed {
                l.state = LegState::Early;
            }
            for d in l.dialogs.iter_mut() {
                d.answered_2xx = None;
                d.awaited_ack_cseq = None;
                if d.state == DialogState::Confirmed {
                    d.state = DialogState::Early;
                }
            }
        }
        reject_early_dialogs(l, 487);
        l.invite_final_sent = Some(487);
    })
}

/// A 2xx to an INVITE the stack sent on a leg. Before the leg's initial
/// INVITE is answered on the wire it is that INVITE's answer: no other INVITE
/// can be in progress on an unconfirmed leg.
fn is_sent_invite_2xx(e: &MessageEntry) -> bool {
    e.direction != MessageDirection::Received
        && e.method == "INVITE"
        && e.code.is_some_and(|c| (200..300).contains(&c))
}

/// Whether a single leg has reached a terminal resolution for termination
/// bookkeeping. A `trying` leg with no `byeDisposition` never established, so it
/// is considered resolved.
///
/// A leg still in `Cancelling` disposition is **not** resolved even though its
/// interim `Cancelled` bye disposition reads terminal: an internal CANCEL is in
/// flight and its transaction has not settled — the callee owes us a `487`, or a
/// `200 OK` may still be crossing our CANCEL on the wire (RFC 3261 §9.1). Holding
/// the leg unresolved keeps the call alive until `resolve-cancel-response` (the
/// 487) or `cancel-200-crossing` (200 → ACK + BYE) reaps the abandoned callee, so
/// finalization (RemoveCall) never strands a ringing b-leg. Both of those rules —
/// and the force-terminal reaper/safety paths — clear the `Cancelling`
/// disposition as they resolve the leg, so this stays unresolved only for the
/// duration of the in-flight CANCEL (bounded by the terminating safety timer).
pub fn leg_is_resolved(leg: &Leg) -> bool {
    if leg.disposition == LegDisposition::Cancelling {
        return false;
    }
    match leg.bye_disposition {
        None => leg.state == LegState::Trying,
        Some(b) => b.is_terminal(),
    }
}

/// Whether a leg — or its whole call — is already going away, so an
/// asynchronous trigger (a timer fire, a transaction timeout, a failure
/// result) must drive NO forward progress through it: no failover consult,
/// no new final on a transaction that already carries one. True when the
/// call's lifecycle has entered `Terminating`/`Terminated`, when the leg's
/// CANCEL is in flight (`Cancelling` — the leg's state still reads `Trying`,
/// so state alone cannot discriminate), or when the leg itself is
/// `Terminated`.
///
/// Distinct from [`leg_is_resolved`]: a `Cancelling` leg is *unresolved*
/// (its callee still owes a 487 or a crossing 200, so finalization waits) yet
/// already going away — resolution and progress-eligibility are different
/// questions.
pub fn leg_is_going_away(call_state: CallModelState, leg: &Leg) -> bool {
    matches!(call_state, CallModelState::Terminating | CallModelState::Terminated)
        || leg.disposition == LegDisposition::Cancelling
        || leg.state == LegState::Terminated
}

/// Whether all legs of a terminating call have reached a terminal resolution
/// (see [`leg_is_resolved`]).
pub fn is_fully_resolved(call: &Call) -> bool {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter()).all(leg_is_resolved)
}

/// Attach a b-leg without recording anything: the event-less attach for
/// fixtures and replica images. A leg this element originates goes through
/// [`super::add_originated_b_leg`], which records its INVITE.
pub fn add_b_leg(mut call: Call, leg: Leg) -> Call {
    call.b_legs.push(leg);
    call
}

/// Find a b-leg by legId.
pub fn find_b_leg<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    call.b_legs.iter().find(|l| l.leg_id == leg_id)
}

/// Find any leg (a-leg or b-leg) by legId.
pub fn find_leg<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    if call.a_leg.leg_id == leg_id {
        Some(&call.a_leg)
    } else {
        find_b_leg(call, leg_id)
    }
}

/// Find a b-leg by callId.
pub fn find_b_leg_by_call_id<'a>(call: &'a Call, call_id: &str) -> Option<&'a Leg> {
    call.b_legs.iter().find(|l| l.call_id == call_id)
}

/// Whether this stack answered the caller: true from the 2xx onward and still
/// true once the leg is `Terminated`. It reads the final the a-leg's initial
/// INVITE server transaction took (`invite_final_sent`, written at the one
/// a-facing final seam), not `LegState::Confirmed`, which the teardown clears.
/// One case turns it back to false: a 2xx the transaction layer's CANCEL 487
/// pre-empted before it reached the wire, which the record then states apart
/// ([`Leg::answer_preempted`]). Replication reconciliation reads the two
/// together ([`super::lifecycle_position`]), so its answer axis never moves
/// back.
pub fn caller_answered(call: &Call) -> bool {
    matches!(call.a_leg.invite_final_sent, Some(status) if (200..300).contains(&status))
}
