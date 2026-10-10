//! Dialog establishment and tracking: b-leg early dialogs (per callee To-tag,
//! forking-aware, minted from the leg's INVITE), 2xx confirmation, the
//! caller-facing dialog fronting each callee dialog, and the a-leg UAS
//! dialogs with their B2BUA-minted tags. Relay itself does NOT live here — see
//! [`super::relay_response`] / [`super::relay_request`].

use call::helpers::{
    callee_dialog, caller_dialog, caller_dialog_mut, fronting_dialog, set_leg_state,
};
use call::{Call, Dialog, DialogState, LegDisposition, LegState, PeerDialog, RetiredDialog};
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
        &self,
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
    /// (RFC 3261 §12.1.2), so a subsequent in-dialog request (PRACK/UPDATE)
    /// can target the callee with the right To-tag. One early dialog per
    /// distinct callee To-tag (downstream forking → several per b-leg), each
    /// from the leg's INVITE ([`call::Leg::outbound_invite`]): its tag, its
    /// CSeq as the start of the dialog's own sequence, and its client
    /// transaction handle (the ACK-for-2xx and RAck CSeq, §13.2.2.4 / RFC 3262
    /// §7.2). Called from the b-leg 1xx/2xx relay path, from
    /// [`Self::ensure_b_early_dialog`] (the suppressed-provisional seam), and
    /// from [`Self::confirm_dialog`] (a 2xx answering under a tag that never
    /// rang). Idempotent per tag; a leg this stack did not dial has no such
    /// dialogs.
    pub(super) fn track_b_early_dialog(
        &self,
        call: &mut Call,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        to_tag: &str,
    ) {
        let Some(leg) = call.b_legs.iter().find(|l| l.leg_id == source_leg_id) else { return };
        let Some(invite) = leg.outbound_invite.clone() else { return };
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
        *call = call::helpers::update_leg(call.clone(), source_leg_id, |leg| {
            leg.state = LegState::Early;
            let mut d = Dialog::new(invite.our_tag, to_tag, DialogState::Early, invite.cseq);
            d.pending_invite_txn = leg.pending_invite_txn.clone();
            d.remote_target = contact;
            d.route_set = route_set;
            leg.dialogs.insert(d);
        });
    }

    /// Confirm a b-leg dialog from the 2xx response event (learn remote tag /
    /// target / CSeq), mark it confirmed+bridged, and ensure the a-leg dialog
    /// exists.
    pub(super) fn confirm_dialog(&self, call: &mut Call, ctx: &RuleContext, leg_id: &str) {
        let resp = match ctx.response() {
            Some(r) => r,
            None => return,
        };
        let remote_tag = resp.to().tag().unwrap_or_default().to_string();
        // A 2xx crossing this leg's CANCEL is reaped (`cancel-200-crossing`),
        // never relayed: it names no dialog the caller is shown.
        let reaped = call
            .b_legs
            .iter()
            .any(|l| l.leg_id == leg_id && l.disposition == LegDisposition::Cancelling);
        let remote_target = contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
            .unwrap_or_default();
        // §12.1.2: the b-leg is a UAC dialog, so its route set is the
        // dialog-creating 2xx's Record-Route values in *reverse* order (the
        // a-leg/UAS path keeps the INVITE's Record-Route forward). We must reverse
        // *individual route URIs*, not header lines: the front proxy double-record-
        // routes (a `;outbound` half + a cookie half), and on the wire those two
        // arrive comma-combined in a single Record-Route header (RFC 3261 §7.3.1).
        // Reversing per-header would be a no-op on that single value and leave the
        // cookie on top — so the worker→callee keepalive carries the cookie first,
        // the proxy decodes it (`w_pri`) and bounces the request back to a worker
        // after a reboot onto a new pod IP the registry has not yet learned (the
        // long-call-loss class). Reading the recorded routes as individual
        // entries puts the proxy's own `;outbound` half on top, so direction is
        // intrinsic to its Record-Route — no `;outbound` worker-stamp and no
        // Via/registry rescue.
        let route_set = self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id);
        // The CSeq the caller's §13.2.2.4 ACK will carry — its own INVITE's.
        let awaited_ack_cseq = relay::rebuild_a_leg_invite(&call.a_leg_invite).cseq().seq() as i64;
        // §12.1.2: a 2xx whose To-tag no provisional ever carried answers on a
        // dialog of its OWN — an accepting branch that never rang. Track it
        // before promotion (idempotent for a known tag) so it exists with the
        // INVITE-seeded CSeq and inherited pending-INVITE handle, instead of
        // inheriting another fork's already-advanced record (§12.2.1.1 skip).
        if !remote_tag.is_empty() {
            self.track_b_early_dialog(call, leg_id, resp, &remote_tag);
        }
        let offered_reliably = call::helpers::offered_in_reliable_provisional(
            call,
            leg_id,
            &remote_tag,
            i64::from(resp.cseq().seq()),
        );
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            // Forking (RFC 3261 §12.1.2): the 2xx confirms exactly ONE early
            // dialog — the one whose callee tag it carries, with the sequence
            // its early PRACK/UPDATE advanced. Every other early dialog retires
            // with its sequence (the BYE releasing a late 2xx of it continues
            // it, §12.2.1.1) and the relays still open in it, whose finals
            // still reach their originators (§8.1.3.3).
            if let Some(d) = call::helpers::callee_dialog_mut(leg, &remote_tag) {
                if !remote_target.is_empty() {
                    d.remote_target = remote_target;
                }
                if !route_set.is_empty() {
                    d.route_set = route_set;
                }
                // The 2xx echoes our INVITE's CSeq, a number of our own
                // sequence: `peer_cseq` keeps the callee's, as its early
                // requests left it (RFC 3261 §12.2.1.1, §12.2.2).
                d.state = DialogState::Confirmed;
                // The session it carries becomes the leg's.
                relay::adopt_confirmed_dialog(leg, &remote_tag);
            }
            for lost in leg.dialogs.iter_mut().filter(|d| d.state == DialogState::Early) {
                let open_relays = std::mem::take(&mut lost.inbound_pending_requests);
                lost.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
            }
            leg.state = LegState::Confirmed;
            leg.disposition = LegDisposition::Bridged;
            // RFC 3261 §13.2.2.4: the ACK this 2xx owes is the caller's own,
            // relayed — arm the obligation for the CSeq that ACK will carry.
            // Its emission mints the ACK's client transaction and retains the
            // branch every later copy of the 2xx is re-ACKed on
            // (`re-ack-retransmitted-2xx`), so no branch exists until then.
            if let Some(d) = leg.dialogs.confirmed_mut() {
                d.awaited_ack_cseq = Some(awaited_ack_cseq);
            }
            // A delayed offer: the INVITE this stack sent carried none, so the
            // 2xx's description is the offer the ACK MUST answer (§13.2.2.4) —
            // unless a reliable provisional of this dialog carried the offer
            // first, whose PRACK carries the answer (RFC 3262 §5).
            let delayed = !offered_reliably
                && leg.dialogs.confirmed().is_some_and(|d| !relay::acked_invite_carries_offer(d));
            if let Some(offer) = resp.sdp().filter(|_| delayed) {
                crate::rules::delayed_offer::note(leg, awaited_ack_cseq, offer);
            }
        }
        // Bridge the a-leg only for a live answer that actually faces the caller,
        // and before its tag is read: the 2xx's a-face is decided as the relay
        // that follows decides it, on a confirmed a-leg. Two guards, both
        // load-bearing:
        //
        // 1. `call.state == Active`. `cancel-200-crossing` reuses `confirm_dialog`
        //    to learn the crossing 200's dialog so it can ACK + BYE the abandoned
        //    callee, but on the reject/no-failover teardown the call is already
        //    `Terminating` and the a-leg was never answered — resurrecting it to
        //    `Confirmed` would read as "answered" and suppress the ADR-0022
        //    unanswered-a-leg final (`invariants::enforce`), stranding the caller
        //    with no response. Leave it as-is (Trying/Early) so the caller still
        //    gets its final when the deferred termination finalizes. The failover
        //    crossing-200 reap runs while the call is still `Active` (awaiting
        //    `/call/failure`), so it keeps confirming the a-leg as before.
        //
        // 2. The confirmed leg is *adopted*. An unadopted `media` leg (an MRF
        //    parked behind early media, ADR-0016) is answered by the *service*,
        //    not relayed to alice — its 2xx surfaced to the caller only as a 183,
        //    so RFC-wise there is no confirmed a-dialog. Confirming the a-leg off
        //    such a 2xx would let a later `BeginTermination` BYE a dialog the
        //    caller never established (undeliverable BYE → the call strands in
        //    `Terminating`, its CDR never flushes). The a-leg is confirmed only
        //    by an a-facing final 2xx — core relay, `RespondToALeg`, or
        //    `AnswerALegNewDialog`. Adopted destination legs (incl. the REFER
        //    transfer target and the failover crossing-200 callee) are unaffected.
        let confirmed_leg_adopted =
            call.b_legs.iter().find(|l| l.leg_id == leg_id).is_none_or(call::helpers::is_adopted);
        let bridges = call.state == call::CallModelState::Active && confirmed_leg_adopted;
        if bridges {
            *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
        }
        // The 2xx that becomes the final to the caller's INVITE: relayed, not
        // reaped, and nothing has answered her yet.
        let answers_the_caller = bridges && !reaped && call.a_leg.invite_final_sent.is_none();
        // The a-facing tag of the dialog this 2xx creates (§12.1.2): the one its
        // relayed provisional minted, the one `MapUnshownDialog` gave a dialog
        // the caller was never shown, or — for a live answer no caller-facing
        // dialog fronts yet and the relay will carry — the one that relay takes
        // (`a_face_tag`).
        let preferred = match fronting_dialog(call, leg_id, &remote_tag) {
            Some(d) => Some(d.our_tag.clone()),
            None if answers_the_caller && !remote_tag.is_empty() => {
                Some(self.a_face_tag(call, leg_id, &remote_tag))
            }
            None => None,
        };
        self.ensure_a_dialog_with(call, preferred.clone());
        // The a-leg may hold dialogs under other tags (an earlier fork's
        // provisional); the 2xx confirms the one the caller sees on it, and
        // retires the others. Only the answer to her INVITE confirms her dialog.
        if let Some(pref) = preferred.filter(|p| !p.is_empty() && answers_the_caller) {
            adopt_a_tag(call, &pref);
        }
        // Keep the answer SDP relayed toward alice on the 2xx as the a-dialog's
        // `cached_sdp` (the relayFirst18x / fake-PRACK cache). The §13.3.1.4
        // repeat does not read it — it re-sends the retained datagram. Mirror
        // the relay body choice (policy override else the callee's 200 body).
        let answer_body = match call.policy_update_body.clone() {
            Some(call::PolicyUpdateBody::Bytes(b)) => Some(b),
            _ if !resp.body().is_empty() => Some(resp.body().to_vec()),
            _ => None,
        };
        if let (Some(body), Some(d)) = (answer_body, own_a_dialog_mut(call)) {
            d.cached_sdp = Some(body);
        }
    }

    /// Show a callee early dialog no provisional showed the caller under a
    /// fresh a-facing To-tag ([`b2bua_sdk::model::RuleAction::MapUnshownDialog`]):
    /// its 2xx then opens a caller dialog of its own (RFC 3261 §12.1.2) — the
    /// relay reads the tag off the fronting dialog, `confirm_dialog` adopts it.
    /// A dialog already fronted keeps the tag it was shown under; a tagless one
    /// names no dialog (§12.1.2 requires the tag) and is left to the primary.
    pub(super) fn map_unshown_dialog(&self, call: &mut Call, b_leg_id: &str, b_tag: &str) {
        if b_tag.is_empty() || fronting_dialog(call, b_leg_id, b_tag).is_some() {
            return;
        }
        let a_tag = self.id_gen.new_tag();
        self.map_a_face(call, &a_tag, b_leg_id, b_tag);
    }

    /// The a-facing tag of the callee dialog `(b_leg_id, b_tag)`: the one of
    /// the caller-facing dialog fronting it, else one minted and fronting it
    /// now. While the caller's INVITE is in setup a transparent relay mirrors
    /// the callee's early dialogs (every one past the first gets its own tag,
    /// RFC 3262 §3/§4); a masking policy, or a confirmed a-leg, mints only for
    /// a second dialog of the same leg and gives a fresh leg the primary.
    pub(super) fn a_face_tag(&self, call: &mut Call, b_leg_id: &str, b_tag: &str) -> String {
        if let Some(d) = fronting_dialog(call, b_leg_id, b_tag) {
            return d.our_tag.clone();
        }
        let primary = self.ensure_a_dialog(call);
        let mirrors_callee_dialogs = call::helpers::relay_first_18x_strategy(call).is_none();
        let in_setup = call.a_leg.state != LegState::Confirmed;
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
        let a_face = if already_published { self.id_gen.new_tag() } else { primary };
        self.map_a_face(call, &a_face, b_leg_id, b_tag);
        a_face
    }

    /// Ensure the a-leg has a dialog of its own with a stable B2BUA-minted tag
    /// ([`call::Leg::own_tag`]); return that tag (the To-tag presented to alice
    /// on every a-facing response no callee dialog names).
    pub(super) fn ensure_a_dialog(&self, call: &mut Call) -> String {
        self.ensure_a_dialog_with(call, None)
    }

    /// Like [`Self::ensure_a_dialog`] but, when the a-dialog is being created,
    /// uses `preferred` as its tag instead of minting a fresh one (tag
    /// continuity across forking/failover, `relayFirst18xTo180`).
    pub(super) fn ensure_a_dialog_with(
        &self,
        call: &mut Call,
        preferred: Option<String>,
    ) -> String {
        if let Some(own) = call.a_leg.own_tag.clone() {
            if caller_dialog(call, &own).is_some() {
                return own;
            }
        }
        let tag = preferred.unwrap_or_else(|| self.id_gen.new_tag());
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
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
        let mut dialog = caller_facing_dialog(&tag, &call.a_leg_invite.from_tag, &a_invite);
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
    pub(super) fn map_a_face(&self, call: &mut Call, a_tag: &str, b_leg_id: &str, b_tag: &str) {
        if call.a_leg.invite_final_sent.is_some()
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
        &self,
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

/// The a-leg's own dialog ([`call::Leg::own_tag`]), to update.
pub(super) fn own_a_dialog_mut(call: &mut Call) -> Option<&mut Dialog> {
    let own = call.a_leg.own_tag.clone()?;
    caller_dialog_mut(call, &own)
}

/// A caller-facing early dialog under our tag `tag` toward the caller's tag
/// `caller_tag`: the remote sequence number is the caller's INVITE CSeq
/// (RFC 3261 §12.1.1), and ours is empty, so our first request toward the
/// caller carries 1 (§12.2.1.1, §8.1.1.5).
fn caller_facing_dialog(tag: &str, caller_tag: &str, a_invite: &sip_message::SipRequest) -> Dialog {
    let mut dialog = Dialog::new(tag, caller_tag, DialogState::Early, 0);
    dialog.peer_cseq = Some(i64::from(a_invite.cseq().seq()));
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
    let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let mut face = caller_facing_dialog(tag, &own.peer_tag, &a_invite);
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
/// relays still reach her (§8.1.3.3).
pub(super) fn adopt_a_tag(call: &mut Call, tag: &str) {
    if caller_dialog(call, tag).is_none() {
        let Some(own) = call.a_leg.own_tag.as_deref().and_then(|t| caller_dialog(call, t)) else {
            return;
        };
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let mut d = caller_facing_dialog(tag, &own.peer_tag, &a_invite);
        d.route_set = own.route_set.clone();
        d.remote_target = own.remote_target.clone();
        call.a_leg.dialogs.insert(d);
    }
    for d in call.a_leg.dialogs.iter_mut() {
        if d.our_tag == tag {
            d.state = DialogState::Confirmed;
        } else if matches!(d.state, DialogState::Early | DialogState::Confirmed) {
            let open_relays = std::mem::take(&mut d.inbound_pending_requests);
            d.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
        }
    }
    call.a_leg.own_tag = Some(tag.to_string());
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
/// Errs when a recorded route does not read; the caller decides, and an empty
/// route set is never that decision (see `ActionExecutor::dialog_route_set`).
pub(super) fn uac_route_set(resp: &sip_message::SipResponse) -> Result<Vec<String>, SipParseError> {
    let mut set = route_texts(resp.list::<RecordRouteEntry>()?);
    set.reverse();
    Ok(set)
}

/// The route set a UAS applies: the requester's recorded routes in the order
/// they were recorded (RFC 3261 §12.1.1). Fallible for the same reason as
/// [`uac_route_set`].
fn uas_route_set(req: &sip_message::SipRequest) -> Result<Vec<String>, SipParseError> {
    Ok(route_texts(req.list::<RecordRouteEntry>()?))
}

/// Recorded routes as the text the `call` crate stores (it has no sip-message
/// dependency, ADR-0008).
fn route_texts(entries: Vec<RecordRouteEntry>) -> Vec<String> {
    entries.into_iter().map(|e| e.to_wire()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::B2buaConfig;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    use sip_txn::IdGen;

    const PROXY_ROUTE: &str = "<sip:10.0.0.9:5060;lr>";

    /// A port no reader accepts (RFC 3261 §19.1.1 `port` is 16-bit): the message
    /// parses, the recorded route does not.
    const UNREADABLE_RR: &str = "<sip:10.0.0.9:70596;lr>";

    fn ok_200(record_route: &str) -> sip_message::SipResponse {
        let raw = format!(
            "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.244.1.5:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.244.2.7:5060>;tag=bob\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:bob@10.244.2.7:5060>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
            SipMessage::Response(r) => r,
            _ => panic!("expected response"),
        }
    }

    fn invite(record_route: &str) -> sip_message::SipRequest {
        let raw = format!(
            "INVITE sip:svc@10.244.1.5:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKa\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@10.0.0.1:5060>;tag=alice\r\n\
To: <sip:svc@10.0.0.9:5060>\r\n\
Call-ID: c1@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: {record_route}\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("parses") {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    fn proxied_config() -> B2buaConfig {
        B2buaConfig {
            b2b_outbound_proxy: Some(("10.0.0.9".to_string(), 5060)),
            ..Default::default()
        }
    }

    // A recorded route no reader accepts must never leave the dialog with an
    // EMPTY route set: an empty set sends in-dialog requests at the peer's
    // Contact (pod-direct), which the deployment forbids. Both dialog sides —
    // the UAC route set read off a 2xx/reliable 1xx and the UAS route set read
    // off the a-leg INVITE — fall back to the configured front proxy instead.
    #[test]
    fn an_unreadable_record_route_never_yields_an_empty_route_set() {
        let config = proxied_config();
        let id_gen = IdGen::seeded(0xD1);
        let wire_faults = crate::wire_faults::WireFaults::none();
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &wire_faults,
        };

        let resp = ok_200(UNREADABLE_RR);
        assert!(uac_route_set(&resp).is_err(), "the fixture's recorded route must not read");
        assert_eq!(
            exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
            vec![PROXY_ROUTE.to_string()],
            "UAC side: an unreadable recorded route routes via the front proxy, not pod-direct",
        );

        let req = invite(UNREADABLE_RR);
        assert!(uas_route_set(&req).is_err(), "the fixture's recorded route must not read");
        assert_eq!(
            exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
            vec![PROXY_ROUTE.to_string()],
            "UAS side: same fallback",
        );
    }

    // With no front proxy configured (local/dev) the transport IS peer-direct,
    // so the fallback is empty — but the read still fails loudly rather than
    // being mistaken for "the peer recorded no route".
    #[test]
    fn without_a_front_proxy_the_fallback_is_empty_but_the_read_still_fails() {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(0xD2);
        let wire_faults = crate::wire_faults::WireFaults::none();
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &wire_faults,
        };
        let resp = ok_200(UNREADABLE_RR);
        assert!(uac_route_set(&resp).is_err());
        assert!(exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1").is_empty());
    }

    // The readable path is unchanged: the UAC reverses the recorded routes
    // (§12.1.2), the UAS keeps them in recorded order (§12.1.1), and a
    // comma-combined line teaches both halves.
    #[test]
    fn readable_recorded_routes_keep_their_dialog_order() {
        let config = proxied_config();
        let id_gen = IdGen::seeded(0xD3);
        let wire_faults = crate::wire_faults::WireFaults::none();
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &wire_faults,
        };
        let combined = "<sip:10.0.0.9:5060;outbound;lr>,<sip:10.0.0.9:5060;target=1;lr>";

        let resp = ok_200(combined);
        assert_eq!(
            exec.dialog_route_set(uac_route_set(&resp), "call-1", "b-1"),
            vec![
                "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
                "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
            ],
        );

        let req = invite(combined);
        assert_eq!(
            exec.dialog_route_set(uas_route_set(&req), "call-1", "a"),
            vec![
                "<sip:10.0.0.9:5060;outbound;lr>".to_string(),
                "<sip:10.0.0.9:5060;target=1;lr>".to_string(),
            ],
        );
    }
}
