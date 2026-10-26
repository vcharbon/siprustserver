//! Dialog establishment and tracking: b-leg early dialogs (per callee To-tag,
//! forking-aware, minted from the leg's INVITE), the caller-facing dialog
//! fronting each callee dialog, and the a-leg UAS dialogs with their
//! B2BUA-minted tags. A 2xx's confirmation is [`super::confirm`]. Relay itself does NOT live here — see
//! [`super::relay_response`] / [`super::relay_request`].

use call::helpers::{callee_dialog, caller_dialog, caller_dialog_mut, fronting_dialog};
use call::{Call, Dialog, DialogState, EndingBy, LegState, PeerDialog, RetiredDialog};
use sip_message::draft::HeaderList;
use sip_message::header::{self, HeaderValue, RecordRouteEntry};
use sip_message::SipParseError;

use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Register the current event's b-leg early dialog when the machine handles
    /// a provisional WITHOUT relaying it (a suppressed fork's reliable 1xx):
    /// `track_b_early_dialog` otherwise only runs on the relay path, so a
    /// suppressed fork would have no `(leg, b_tag)` dialog and its AS-originated
    /// PRACK / SDP cache — keyed strictly on that pair — would silently miss.
    /// Only the event's own response can seed the dialog (Contact /
    /// Record-Route / CSeq come from it); other `(leg, tag)` targets must
    /// already be tracked. Idempotent (`track_b_early_dialog` skips a known
    /// tag).
    pub(super) fn ensure_b_early_dialog(
        &mut self,
        call: &mut Call,
        ctx: &RuleContext,
        leg_id: &str,
        b_tag: &str,
    ) {
        if b_tag.is_empty() {
            return;
        }
        let Some(resp) = ctx.response() else { return };
        if ctx.source_leg_id != leg_id || resp.to().tag() != Some(b_tag) {
            return;
        }
        self.track_b_early_dialog(call, leg_id, resp, b_tag);
    }

    /// Mint the early dialog a tagged response to a b-leg's INVITE creates
    /// (RFC 3261 §12.1.2) from the leg's INVITE: its tag, its CSeq as the
    /// start of the dialog's sequence, its client transaction (§13.2.2.4,
    /// RFC 3262 §7.2). Idempotent per tag; a leg this stack did not dial has
    /// none.
    pub(super) fn track_b_early_dialog(
        &mut self,
        call: &mut Call,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        to_tag: &str,
    ) {
        let Some(leg) = call.b_legs.iter().find(|l| l.leg_id == source_leg_id) else { return };
        let Some(invite) = leg.outbound_invite().cloned() else { return };
        let cseq = leg.dialled_cseq().unwrap_or_default();
        let sent_invite = leg.dialled_on_dialog();
        if callee_dialog(leg, to_tag).is_some() {
            return;
        }
        let contact = contact_uri(resp.header::<header::Contact>(), &call.call_ref, source_leg_id)
            .or_else(|| leg.invite_request_uri.clone())
            .unwrap_or_default();
        // §12.1.2: an EARLY dialog's route set is established from the reliable
        // 1xx's Record-Route, exactly like the 2xx path below — one entry per
        // recorded route (a comma-combined double-record-route is two), reversed
        // (UAC side). Without this a PRACK/UPDATE on the early dialog rides the
        // preloaded bootstrap Route only and under-reproduces the route set (the
        // §12.2.1.1 audit catches it behind a front proxy).
        let route_set = self.dialog_route_set(uac_route_set(resp), &call.call_ref, source_leg_id);
        // An early dialog opened behind this stack's CANCEL of the INVITE is
        // ending from the start (RFC 3261 §9.1).
        let state = if leg.cancel_sent() {
            DialogState::Ending(EndingBy::Cancel)
        } else {
            DialogState::Early
        };
        *call = call::helpers::update_leg(call.clone(), source_leg_id, |leg| {
            let mut d = Dialog::new(invite.our_tag, to_tag, state, cseq);
            d.sent_invite = sent_invite;
            d.remote_target = contact;
            d.route_set = route_set;
            leg.dialogs.insert(d);
        });
    }

    /// Show a callee early dialog no provisional showed the caller under a
    /// fresh a-facing To-tag ([`b2bua_sdk::model::RuleAction::MapUnshownDialog`]):
    /// its 2xx then opens a caller dialog of its own (RFC 3261 §12.1.2) — the
    /// relay reads the tag off the fronting dialog, `confirm` adopts it.
    /// A dialog already fronted keeps the tag it was shown under; a tagless one
    /// names no dialog (§12.1.2 requires the tag) and is left to the primary.
    pub(super) fn map_unshown_dialog(&mut self, call: &mut Call, b_leg_id: &str, b_tag: &str) {
        if b_tag.is_empty() || fronting_dialog(call, b_leg_id, b_tag).is_some() {
            return;
        }
        let a_tag = crate::caller_tags::fronting(call, b_leg_id, b_tag);
        self.map_a_face(call, &a_tag, b_leg_id, b_tag);
    }

    /// The a-facing tag of the callee dialog `(b_leg_id, b_tag)`: the one of
    /// the caller-facing dialog fronting it, else one minted and fronting it
    /// now. While the caller's INVITE is in setup a transparent relay mirrors
    /// the callee's early dialogs (every one past the first gets its own tag,
    /// RFC 3262 §3/§4); a masking policy, a confirmed a-leg, or an `answering`
    /// response (a 2xx toward her) mints only for a second dialog of the same
    /// leg and gives a fresh leg the primary.
    pub(super) fn a_face_tag(
        &mut self,
        call: &mut Call,
        b_leg_id: &str,
        b_tag: &str,
        answering: bool,
    ) -> String {
        if let Some(d) = fronting_dialog(call, b_leg_id, b_tag) {
            return d.our_tag.clone();
        }
        let primary = self.ensure_a_dialog(call);
        let mirrors_callee_dialogs = call::helpers::relay_first_18x_strategy(call).is_none();
        let in_setup = !answering && call.a_leg.state() != LegState::Confirmed;
        let already_published = {
            let mut fronts = call
                .a_leg
                .dialogs
                .iter()
                .filter(|d| d.retired().is_none())
                .flat_map(|d| d.fronts.iter());
            if mirrors_callee_dialogs && in_setup {
                fronts.next().is_some()
            } else {
                fronts.any(|f| f.leg_id == b_leg_id)
            }
        };
        let a_face = if already_published || primary_shows_media(call, &primary) {
            crate::caller_tags::fronting(call, b_leg_id, b_tag)
        } else {
            primary
        };
        self.map_a_face(call, &a_face, b_leg_id, b_tag);
        a_face
    }

    /// The tag a 2xx of `b_leg_id`, a leg the record lacks, was relayed to the
    /// caller under, as [`Self::a_face_tag`] chose it for an answer from a
    /// leg nothing fronts yet: the fronting tag beside a media leg's early
    /// media, else the primary. Its early dialog is opened under it.
    pub(super) fn unknown_leg_face_tag(
        &mut self,
        call: &mut Call,
        b_leg_id: &str,
        b_tag: &str,
    ) -> String {
        let primary = self.ensure_a_dialog(call);
        if !primary_shows_media(call, &primary) {
            return primary;
        }
        let a_face = crate::caller_tags::fronting(call, b_leg_id, b_tag);
        open_early_a_dialog(call, &a_face);
        a_face
    }

    /// The caller-facing tag an early-media provisional whose body `author`
    /// wrote is shown under. `stated` → the service states the caller's early
    /// identity: it seeds the a-dialog, re-stamps one a b-leg confirm minted
    /// before any caller-facing response carried it, and beside a tag the
    /// caller was already shown opens an early dialog of its own (RFC 3261
    /// §12.1.2). Absent → [`Self::media_face_tag`]. Either way a media leg's
    /// early media holds its dialog alone (RFC 3264 §8).
    pub(super) fn early_media_tag(
        &mut self,
        call: &mut Call,
        stated: Option<&str>,
        author: relay::Author<'_>,
    ) -> String {
        let media = match author {
            relay::Author::Leg(leg) if leg != call.a_leg.leg_id => Some(leg),
            _ => None,
        };
        let Some(t) = stated else {
            return match media {
                Some(media) => self.media_face_tag(call, media),
                None => self.ensure_a_dialog(call),
            };
        };
        let primary = self.ensure_a_dialog_with(call, Some(t.to_string()));
        let held = caller_dialog(call, t).is_some();
        if !held && call::helpers::a_tag_shown(call, &primary) {
            open_early_a_dialog(call, t);
        } else if !held {
            // The a-leg's own dialog was never shown: the caller knows it
            // under no tag, so it takes the stated one.
            if let Some(d) = own_a_dialog_mut(call) {
                d.our_tag = t.to_string();
            }
            call.a_leg.own_tag = Some(t.to_string());
        }
        if let Some(media) = media {
            show_media(call, t, media);
        }
        t.to_string()
    }

    /// The caller-facing tag the early media of the media leg `media_leg` is
    /// shown under when the service states none: the dialog already showing
    /// it; else the a-leg's own dialog where the caller was shown nothing
    /// under it and it fronts no callee, so a non-2xx final still answers
    /// under a tag she holds (RFC 3261 §17.2.1); else an early dialog of its
    /// own beside the tags she holds. Either way that dialog shows the media
    /// leg alone ([`Dialog::shows_media`]): a callee's provisionals and its 2xx go out under
    /// another tag ([`Self::a_face_tag`]), and the 2xx ends the media leg's
    /// early dialog (RFC 3261 §12.1.2, RFC 3264 §8).
    pub(super) fn media_face_tag(&mut self, call: &mut Call, media_leg: &str) -> String {
        let showing = call
            .a_leg
            .dialogs
            .iter()
            .find(|d| d.is_live() && d.shows_media.as_deref() == Some(media_leg));
        if let Some(d) = showing {
            return d.our_tag.clone();
        }
        let primary = self.ensure_a_dialog(call);
        let free = caller_dialog(call, &primary)
            .is_some_and(|d| !d.shown && d.fronts.is_empty() && d.shows_media.is_none());
        let tag = if free { primary } else { crate::caller_tags::of_leg(call, media_leg) };
        open_early_a_dialog(call, &tag);
        show_media(call, &tag, media_leg);
        tag
    }

    /// Ensure the a-leg has a dialog of its own with a stable B2BUA-minted tag
    /// ([`call::Leg::own_tag`]); return that tag (the To-tag presented to alice
    /// on every a-facing response no callee dialog names).
    pub(super) fn ensure_a_dialog(&mut self, call: &mut Call) -> String {
        self.ensure_a_dialog_with(call, None)
    }

    /// Like [`Self::ensure_a_dialog`] but, when the a-dialog is being created,
    /// uses `preferred` as its tag instead of minting a fresh one (tag
    /// continuity across forking/failover, `relayFirst18xTo180`). A caller
    /// whose INVITE took a non-2xx final holds no dialog of it (RFC 3261
    /// §12.1): none is opened then, and the tag returned names none.
    pub(super) fn ensure_a_dialog_with(
        &mut self,
        call: &mut Call,
        preferred: Option<String>,
    ) -> String {
        if let Some(own) = call.a_leg.own_tag.clone() {
            if caller_dialog(call, &own).is_some() {
                return own;
            }
        }
        if call.a_leg.invite_final_sent().is_some_and(|status| !(200..300).contains(&status)) {
            return call.a_leg.own_tag.clone().or(preferred).unwrap_or_default();
        }
        let tag = preferred.unwrap_or_else(|| crate::caller_tags::own(call));
        let Some(a_invite) = self.caller_invite(call) else { return tag };
        let from = a_invite.from();
        let a_leg_id = call.a_leg.leg_id.clone();
        let remote_target =
            contact_uri(a_invite.header::<header::Contact>(), &call.call_ref, &a_leg_id)
                .unwrap_or_else(|| from.uri().to_string());
        // §12.1.1: the a-leg is a UAS dialog — route set is the INVITE's
        // Record-Route entries in forward order, one entry per recorded route (a
        // comma-combined header — the proxy's double-record-route halves — is
        // two), same as the b-leg path above.
        let route_set = self.dialog_route_set(uas_route_set(&a_invite), &call.call_ref, &a_leg_id);
        let invite_cseq = i64::from(a_invite.cseq().seq());
        let mut dialog = caller_facing_dialog(&tag, &call.a_leg_invite.from_tag, invite_cseq);
        dialog.remote_target = remote_target;
        dialog.route_set = route_set;
        call.a_leg.dialogs.insert(dialog);
        call.a_leg.own_tag = Some(tag.clone());
        tag
    }

    /// Show the callee dialog `(b_leg_id, b_tag)` to the caller under `a_tag`:
    /// the caller-facing dialog of that tag fronts it, opened beside the
    /// a-leg's own dialog when the a-leg does not hold it yet
    /// ([`open_early_a_dialog`]), or as the a-leg's own dialog when it holds
    /// none. Nothing is shown once the caller's INVITE has its final, nor for
    /// a callee dialog already fronted.
    pub(super) fn map_a_face(&mut self, call: &mut Call, a_tag: &str, b_leg_id: &str, b_tag: &str) {
        if call.a_leg.invite_final_sent().is_some()
            || a_tag.is_empty()
            || fronting_dialog(call, b_leg_id, b_tag).is_some()
        {
            return;
        }
        if call.a_leg.own_tag.is_none() {
            self.ensure_a_dialog_with(call, Some(a_tag.to_string()));
        }
        open_early_a_dialog(call, a_tag);
        if let Some(d) = caller_dialog_mut(call, a_tag) {
            d.fronts.push(PeerDialog { leg_id: b_leg_id.to_string(), peer_tag: b_tag.to_string() });
        }
    }

    /// The route set to store on a dialog, given a read of the peer's recorded
    /// routes. A recorded route no reader accepts must NOT become an empty
    /// route set: an empty set sends every in-dialog request straight at the
    /// peer's Contact — pod-direct — while the deployment requires every
    /// worker-originated request to traverse the front proxy (a peer's pod IP
    /// is not routable peer-to-peer, so the call is lost the moment it moves).
    /// Such a read falls back to the configured outbound proxy as the dialog's
    /// one route and names the call on stderr; with no proxy configured
    /// (local/dev, where the transport IS peer-direct) the set stays empty.
    pub(super) fn dialog_route_set(
        &mut self,
        read: Result<Vec<String>, SipParseError>,
        call_ref: &str,
        leg_id: &str,
    ) -> Vec<String> {
        match read {
            Ok(set) => set,
            Err(err) => {
                let fallback = relay::outbound_proxy_route_set(self.config);
                let route = fallback.first().map(String::as_str).unwrap_or("<none configured>");
                tracing::warn!(
                    %call_ref,
                    %leg_id,
                    error = %err,
                    %route,
                    "a recorded route does not read; the dialog route set falls back to the \
                     outbound proxy — an empty route set would send in-dialog requests pod-direct"
                );
                fallback
            }
        }
    }
}

/// Whether the caller's dialog `primary` carries a media leg's early media:
/// a callee's dialog is then another session, under another tag (RFC 3264 §8).
fn primary_shows_media(call: &Call, primary: &str) -> bool {
    caller_dialog(call, primary).is_some_and(|d| d.shows_media.is_some())
}

/// Record that the caller-facing dialog `tag` shows the early media of the
/// media leg `media_leg` ([`Dialog::shows_media`]), so no callee is shown
/// under it ([`ActionExecutor::a_face_tag`]). Nothing for a leg that is not a
/// media leg. Request routing on the dialog is untouched.
fn show_media(call: &mut Call, tag: &str, media_leg: &str) {
    let media = call
        .b_legs
        .iter()
        .any(|l| l.leg_id == media_leg && call::helpers::leg_kind(l) == call::LegKind::Media);
    if let Some(d) = caller_dialog_mut(call, tag).filter(|_| media) {
        d.shows_media = Some(media_leg.to_string());
    }
}

/// The a-leg's own dialog ([`call::Leg::own_tag`]), to update.
pub(super) fn own_a_dialog_mut(call: &mut Call) -> Option<&mut Dialog> {
    let own = call.a_leg.own_tag.clone()?;
    caller_dialog_mut(call, &own)
}

/// A caller-facing early dialog under our tag `tag` toward the caller's tag
/// `caller_tag`: the remote sequence number is the caller's INVITE CSeq
/// (RFC 3261 §12.1.1), and ours is empty, so our first request toward the
/// caller carries 1 (§12.2.1.1, §8.1.1.5).
fn caller_facing_dialog(tag: &str, caller_tag: &str, invite_cseq: i64) -> Dialog {
    let mut dialog = Dialog::new(tag, caller_tag, DialogState::Early, 0);
    dialog.peer_cseq = Some(invite_cseq);
    dialog
}

/// Open the caller-facing early dialog `tag` names beside the a-leg's own
/// dialog: a record of its own — same caller, route set and target — with
/// fresh sequences ([`caller_facing_dialog`], RFC 3261 §12.1.2). Nothing is
/// opened for an empty tag, a tag the a-leg already
/// holds, or an a-leg with no dialog of its own yet.
pub(super) fn open_early_a_dialog(call: &mut Call, tag: &str) {
    if tag.is_empty() || caller_dialog(call, tag).is_some() {
        return;
    }
    let Some(own) = call.a_leg.own_tag.as_deref().and_then(|t| caller_dialog(call, t)) else {
        return;
    };
    let mut face = caller_facing_dialog(tag, &own.peer_tag, i64::from(call.a_leg_invite.cseq));
    face.route_set = own.route_set.clone();
    face.remote_target = own.remote_target.clone();
    call.a_leg.dialogs.insert(face);
}

/// Confirm the caller's answered dialog, the one whose tag is `tag`: the
/// caller-facing dialog of that tag, or one opened now with fresh sequences
/// for a tag never shown. It becomes the a-leg's own dialog. Every other
/// caller-facing dialog the answer supersedes — early, or confirmed under an
/// answer the caller was never sent — retires with the relays still open in
/// it, so a
/// request the caller sends on an early dialog the answer abandoned matches no
/// live dialog and draws `481` (RFC 3261 §12.2.2), while the finals of those
/// relays still reach her (§8.1.3.3). The answered dialog's session state
/// becomes the leg's.
pub(super) fn adopt_a_tag(call: &mut Call, tag: &str) {
    if caller_dialog(call, tag).is_none() {
        let Some(own) = call.a_leg.own_tag.as_deref().and_then(|t| caller_dialog(call, t)) else {
            return;
        };
        let mut d = caller_facing_dialog(tag, &own.peer_tag, i64::from(call.a_leg_invite.cseq));
        d.route_set = own.route_set.clone();
        d.remote_target = own.remote_target.clone();
        call.a_leg.dialogs.insert(d);
    }
    for d in call.a_leg.dialogs.iter_mut() {
        if d.our_tag == tag {
            d.state = DialogState::Confirmed;
        } else if matches!(d.state, DialogState::Early | DialogState::Confirmed) {
            let open_requests = std::mem::take(&mut d.requests);
            d.state = DialogState::Retired(RetiredDialog { open_requests });
        }
    }
    call.a_leg.own_tag = Some(tag.to_string());
    // The answered dialog's session is the leg's from now on (RFC 3264 §8).
    let caller_tag = call.a_leg_invite.from_tag.clone();
    relay::adopt_answered_caller_dialog(&mut call.a_leg, tag, &caller_tag);
}

/// The dialog's remote target: the URI of the peer's Contact (RFC 3261
/// §12.1.1/§12.1.2). `None` when the peer sent none; a Contact no reader
/// accepts is named on stderr and leaves the dialog's current target in place
/// rather than silently retargeting it at nothing.
pub(super) fn contact_uri(
    contact: Option<Result<header::Contact, SipParseError>>,
    call_ref: &str,
    leg_id: &str,
) -> Option<String> {
    match contact? {
        Ok(c) => Some(c.uri().to_string()),
        Err(err) => {
            tracing::warn!(
                %call_ref,
                %leg_id,
                error = %err,
                "Contact does not read; keeping the dialog's current remote target"
            );
            None
        }
    }
}

/// The route set a UAC applies: the responder's recorded routes reversed
/// (RFC 3261 §12.1.2). One entry per recorded route, so a comma-combined line —
/// the front proxy's double record-route — yields both halves in wire order.
/// Errs when a recorded route does not read or is no request's target; the
/// caller decides, and an empty route set is never that decision (see
/// `ActionExecutor::dialog_route_set`).
pub(super) fn uac_route_set(resp: &sip_message::SipResponse) -> Result<Vec<String>, SipParseError> {
    Ok(route_texts(resp.record_route_set()?.reversed()))
}

/// The route set a UAS applies: the requester's recorded routes in the order
/// they were recorded (RFC 3261 §12.1.1). Fallible for the same reason as
/// [`uac_route_set`].
fn uas_route_set(req: &sip_message::SipRequest) -> Result<Vec<String>, SipParseError> {
    Ok(route_texts(req.record_route_set()?))
}

/// Recorded routes as the text the `call` crate stores (it has no sip-message
/// dependency, ADR-0008).
fn route_texts(entries: HeaderList<RecordRouteEntry>) -> Vec<String> {
    entries.iter().map(HeaderValue::to_wire).collect()
}

#[cfg(test)]
#[path = "dialog_track_tests.rs"]
mod tests;
