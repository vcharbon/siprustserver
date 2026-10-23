//! Each dialog's session state (RFC 3264 §8 per dialog, RFC 3261 §12.1.2):
//! an early dialog keeps its own until one confirms and hands it to its leg,
//! and a description the stack composes inside a dialog continues the session
//! the stack stated there.

use call::{Call, Dialog, DialogId, Leg, LegSdpSession, LegState};

use super::sdp_session::{leg_mut, origin_of};

/// The early dialog `id` of `leg`: on the caller's leg (`caller`) by this
/// stack's tag, its early dialogs sharing the caller's; on a leg this stack
/// dialled by the peer's.
pub(super) fn early_dialog_mut<'a>(
    leg: &'a mut Leg,
    caller: bool,
    id: &DialogId,
) -> Option<&'a mut Dialog> {
    if caller {
        leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag)
    } else {
        call::helpers::callee_dialog_mut(leg, &id.peer_tag)
    }
}

/// Puts the state early dialog `id` of the unconfirmed leg `leg_id` carries in
/// the leg's place and returns the leg's own (the opening state), or `None`
/// where `leg_id` is not an unconfirmed leg with such a dialog.
pub(super) fn enter_early_dialog(
    call: &mut Call,
    leg_id: &str,
    caller: bool,
    id: &DialogId,
) -> Option<LegSdpSession> {
    let leg = leg_mut(call, leg_id)?;
    if leg.dialogs.confirmed().is_some() {
        return None;
    }
    let mut own = early_dialog_mut(leg, caller, id)?.sdp_session.take();
    if let Some(own) = own.as_mut() {
        own.exchanges_opened = leg.sdp_session.exchanges_opened;
    }
    Some(match own {
        Some(own) => std::mem::replace(&mut leg.sdp_session, own),
        None => leg.sdp_session.clone(),
    })
}

/// The session state the early dialog with peer tag `peer_tag` carries, handed
/// to `leg` as that dialog confirms it; nothing where the dialog carries the
/// opening state. Whatever that dialog carries, the leg keeps the restatements
/// its other forks and its own state carried: the call's fact outlives the
/// retired forks.
pub fn adopt_confirmed_dialog(leg: &mut Leg, peer_tag: &str) {
    let id = DialogId { our_tag: String::new(), peer_tag: peer_tag.to_string() };
    adopt_dialog(leg, false, &id);
}

/// [`adopt_confirmed_dialog`] for the caller's leg: the caller-facing dialog
/// under this stack's tag `our_tag` is the one the answer confirmed.
pub fn adopt_answered_caller_dialog(leg: &mut Leg, our_tag: &str, caller_tag: &str) {
    let id = DialogId { our_tag: our_tag.to_string(), peer_tag: caller_tag.to_string() };
    adopt_dialog(leg, true, &id);
}

fn adopt_dialog(leg: &mut Leg, caller: bool, id: &DialogId) {
    let restated = leg.sdp_session.has_restated
        || leg.dialogs.iter().any(|d| d.sdp_session.as_ref().is_some_and(|s| s.has_restated));
    if let Some(mut own) = early_dialog_mut(leg, caller, id).and_then(|d| d.sdp_session.take()) {
        own.exchanges_opened = leg.sdp_session.exchanges_opened;
        own.absorbed_origin = leg.sdp_session.absorbed_origin.take();
        leg.sdp_session = own;
    }
    leg.sdp_session.has_restated |= restated;
}

/// The `o=` value of the next version of the session `leg`'s dialog whose
/// remote tag is `remote_tag` carries: the last one this stack stated there,
/// one up. `None` where the stack stated none there, or the version has no
/// next one.
pub fn next_origin_in_dialog(leg: &Leg, remote_tag: &str) -> Option<String> {
    match call::helpers::callee_dialog(leg, remote_tag) {
        Some(dialog) => next_origin_on(leg, dialog),
        None => next_version(&leg.sdp_session),
    }
}

/// [`next_origin_in_dialog`] for `dialog`, the dialog of `leg` a message
/// names, on either side of the call: its own state while `leg` is
/// unconfirmed and it carries one, the leg's otherwise.
pub fn next_origin_on(leg: &Leg, dialog: &Dialog) -> Option<String> {
    let early = (leg.state() != LegState::Confirmed).then_some(dialog.sdp_session.as_ref());
    next_version(early.flatten().unwrap_or(&leg.sdp_session))
}

fn next_version(state: &LegSdpSession) -> Option<String> {
    let line = origin_of(state.sent_origin.as_deref()?)?.next_version_line()?;
    Some(line.strip_prefix("o=")?.to_string())
}

/// `sdp`, a description the stack composed, stated under `origin` — the `o=`
/// value of the next version of the session the stack stated on the dialog
/// ([`next_origin_in_dialog`]) — where there is one; as composed otherwise.
pub fn in_dialog_session(sdp: Vec<u8>, origin: Option<&str>) -> Option<Vec<u8>> {
    match origin {
        Some(origin) => sip_message::under_origin(&sdp, origin).or(Some(sdp)),
        None => Some(sdp),
    }
}
