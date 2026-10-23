//! RFC 7315 §5.6 charging correlation on the legs a call originates: the
//! originated-leg arm ([`call::features::ChargingVectorFeature`]) mints a
//! vector on a leg's INVITE where none reached the stack. A call whose
//! decision states the vector for that INVITE ([`call::features::StatedHeaders`])
//! mints none: the decision outranks the arm, and the stated headers carry it
//! (`rules::stated_headers`). A deployment that leaves media legs uncharged
//! ([`call::features::FeatureActivations::uncharged_media_legs`]) has neither
//! a mint nor a relayed vector on a media leg's INVITE ([`uncharge_media_leg`]).
//! An arm with `in_dialog_invites` also puts the leg's own vector on the
//! re-INVITEs the stack sends on its own behalf ([`in_dialog_invite_vector`]).

use call::features::ChargingVectorFeature;
use call::{Call, LegKind};
use sip_message::header::{ChargingVector, HeaderValue};

/// The arm the leg `leg_id` of `kind` this call originates mints its own
/// vector under: none where the set that leg takes (its own, else the call's)
/// states the vector for its INVITE, or where the deployment leaves a media
/// leg uncharged.
pub fn minting_arm<'a>(
    call: &'a Call,
    leg_id: &str,
    kind: Option<LegKind>,
) -> Option<&'a ChargingVectorFeature> {
    let features = call.features.as_ref()?;
    if uncharged(call, kind) {
        return None;
    }
    let name = ChargingVector::header_name();
    if features.stated_headers.as_ref().is_some_and(|s| s.of_leg(leg_id).states(name.as_wire_str()))
    {
        return None;
    }
    features.charging_vector.as_ref()
}

/// True iff `call`'s deployment leaves a leg of `kind` uncharged.
fn uncharged(call: &Call, kind: Option<LegKind>) -> bool {
    kind == Some(LegKind::Media) && call.features.as_ref().is_some_and(|f| f.uncharged_media_legs)
}

/// The media leg's INVITE `effect` mints on `leg` with no `P-Charging-Vector`
/// where the deployment leaves media legs uncharged and the decision's
/// `header_updates` state none for it; the leg's handles hold the INVITE as it
/// leaves.
pub fn uncharge_media_leg(
    call: &Call,
    kind: Option<LegKind>,
    header_updates: &[(String, Option<String>)],
    leg: &mut call::Leg,
    effect: &mut crate::effects::OutboundSipEffect,
) {
    let name = ChargingVector::header_name();
    if !uncharged(call, kind) || header_updates.iter().any(|(n, _)| name.matches(n)) {
        return;
    }
    if let crate::effects::OutboundBody::Request(req) = &mut effect.body {
        if req.raw(name.clone()).next().is_none() {
            return;
        }
        let was = req.image().to_vec();
        if let Ok(stripped) = req.thaw().remove(&name).freeze() {
            *req = stripped;
            call::helpers::restate_leg_invite(leg, &was, req.image());
        }
    }
}

/// The vector line a re-INVITE the stack sends on its own behalf on `leg_id`
/// carries, where the call's arm carries `in_dialog_invites`: the vector that
/// leg's dialog-creating INVITE carried (the originator's own on the
/// originator's leg), none where it carried none or the leg is an uncharged
/// media leg.
pub fn in_dialog_invite_vector(call: &Call, leg_id: &str) -> Option<String> {
    call.features
        .as_ref()
        .and_then(|f| f.charging_vector.as_ref())
        .filter(|a| a.in_dialog_invites)?;
    let leg = call::helpers::find_leg(call, leg_id)?;
    let kind = call::helpers::leg_kind(leg);
    if uncharged(call, Some(kind)) {
        return None;
    }
    let name = ChargingVector::header_name();
    if kind == LegKind::A {
        return call
            .a_leg_invite
            .headers
            .iter()
            .find(|h| name.matches(&h.name))
            .map(|h| h.value.clone());
    }
    let dialled = crate::rules::relay::dialling_invite(leg)?;
    let line = dialled.raw(name).next().map(str::to_string);
    line
}
