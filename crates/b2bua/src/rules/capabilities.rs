//! Which capability set the B2BUA advertises on each face of a call.
//!
//! The advertisement is call-scoped policy: the decision engine declares it in
//! `features.advertise_capabilities`, the declaration replicates with the call,
//! and every mint point reads the face it is emitting on through this module.
//!
//! Precedence at every mint point, most specific first: an explicit header
//! update carried on the message (a decision's `header_updates`, a firing
//! rule's own `Allow`/`Supported`) beats the call's declared set for that face,
//! which beats the set RELAYED from the peer (RFC 3261 §16.6, [`relaying`]),
//! and where nothing was declared and nothing was received the face states
//! NO line. A re-INVITE the stack originates has no peer message to relay
//! from: on a leg it dialled, its `Accept` restates the one the dialling
//! INVITE stated as it left, edits applied ([`for_reinvite`]), and toward the
//! originator it states none. An advertisement is a claim about the party that makes it: a
//! back-to-back UA relays the peer's verbatim and never widens, narrows or
//! invents one, and what a peer left unsaid stays unsaid. The one claim the
//! stack states on its own behalf is an extension it exercises itself: an
//! INVITE it originates under the `fake-prack` strategy offers `100rel`,
//! because the stack — not the originator — acknowledges the reliable
//! provisionals that offer solicits ([`offered_option_tags`]); and the one
//! narrowing it makes on its own behalf is the twin of that offer: a strategy
//! that acknowledges none withholds `100rel` from every leg it originates
//! ([`withheld_by_strategy_in`]). The stack's own set
//! ([`CapabilitySet::default`]) is for the messages the stack answers on its
//! own behalf — the out-of-dialog OPTIONS — and for nothing it relays. A
//! relayed value is not an explicit update and never outranks a declaration:
//! the messages that carry one, the relayed requests, drop it (see
//! [`declared_advert_headers`]).
//!
//! The halves resolve independently — an undeclared `Allow` is relayed while
//! a declared `Supported` narrows the option tags — and an empty declared half
//! advertises the empty set rather than nothing.
//!
//! A DECLARED set is read through the token grammar, so nothing it states can
//! reach the wire as anything but option tags. An explicit `header_updates`
//! line is not: it states its own bytes verbatim, and the decision layer owns
//! their well-formedness.

use call::features::{AdvertisedCapabilities, FeatureActivations, RelayFirst18xStrategy};
use call::{Call, LegKind};
use sip_message::generators::CapabilitySet;
use sip_message::header::{Allow, HeaderName, Supported};
use sip_message::{SipHeader, SipRequest, SipStr};

/// The face of the back-to-back UA an advertisement is emitted on. The two are
/// declared independently, so a bridge between asymmetric domains can narrow
/// one of them without touching the other. A call with several originated legs
/// (a media leg plus a destination leg) has ONE originated face: the granularity
/// is the face, not the leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    /// Toward the originator — the a-leg / caller.
    Originator,
    /// Toward a leg the B2BUA originated — a b-leg / callee.
    Originated,
}

impl Face {
    /// The face `leg_id` sits on: `"a"` is the originator's leg, every other
    /// leg is one the B2BUA originated.
    pub fn of_leg(leg_id: &str) -> Self {
        if leg_id == "a" {
            Face::Originator
        } else {
            Face::Originated
        }
    }
}

/// The set `features` DECLARE for `face`, or `None` when they declare none. A
/// service that owns a different default reads this and keeps its own value
/// when nothing is declared. An undeclared HALF of a declared face carries no
/// line of its own — on a relayed message it is the peer's, on a minted one
/// it is absent — so a declaration narrows exactly what it names.
pub fn declared_in(features: Option<&FeatureActivations>, face: Face) -> Option<CapabilitySet> {
    Some(typed(declared_face(features, face)?))
}

/// The advertisement headers `features` DECLARE for `face`, empty when they
/// declare neither half.
///
/// A relayed request carries the other peer's `Allow`/`Supported` through (RFC
/// 3261 §16.6); that copy would defeat a declared narrowing, so the relay drops
/// exactly these names — and leaves the transparent relay alone for a half
/// nothing declares.
pub fn declared_advert_headers(
    features: Option<&FeatureActivations>,
    face: Face,
) -> Vec<HeaderName> {
    b2bua_sdk::in_dialog_relay::declared_halves(features, face == Face::Originator)
}

/// The face's declaration inside `features`, if any.
fn declared_face(
    features: Option<&FeatureActivations>,
    face: Face,
) -> Option<&AdvertisedCapabilities> {
    b2bua_sdk::in_dialog_relay::declared_face(features, face == Face::Originator)
}

/// The set the call DECLARED for `face`, or `None` when it declared none.
pub fn declared(call: &Call, face: Face) -> Option<CapabilitySet> {
    declared_in(call.features.as_ref(), face)
}

/// The set the B2BUA states of its own on `face`, beside whatever peer
/// advertisement a message carries through: the declared one, else no line
/// at all.
pub fn advertised(call: &Call, face: Face) -> CapabilitySet {
    declared(call, face).unwrap_or_else(CapabilitySet::silent)
}

/// [`advertised`] on whichever face `leg_id` sits on.
pub fn for_leg(call: &Call, leg_id: &str) -> CapabilitySet {
    advertised(call, Face::of_leg(leg_id))
}

/// The set a re-INVITE the B2BUA originates on `leg_id` states, `creating`
/// being the INVITE that created the leg's dialog (the one the stack sent on a
/// leg it dialled, the originator's own on hers). `Allow` is the declared half,
/// else the one `creating` carried: a UA's method set holds for the dialog and
/// §13.2.1 asks an INVITE to state it. `Supported` is the declared half alone.
/// `Accept` (RFC 3261 §20.1) is the one the dialling INVITE stated on a leg the
/// stack dialled, so that peer reads one statement of acceptable bodies over
/// the dialog's life; toward the originator, none.
pub fn for_reinvite(call: &Call, leg_id: &str, creating: Option<&SipRequest>) -> CapabilitySet {
    let own = for_leg(call, leg_id);
    let carried = creating.map(|invite| CapabilitySet::relayed(invite.headers()));
    let allow = own.allow().cloned().or_else(|| carried.as_ref()?.allow().cloned());
    let accept = match Face::of_leg(leg_id) {
        Face::Originator => None,
        Face::Originated => carried.as_ref().and_then(|c| c.accept().map(<[_]>::to_vec)),
    };
    CapabilitySet::stating(allow, own.supported().cloned(), accept)
}

/// The set advertised on `face` for a message that carries the peer's own
/// advertisement across the back-to-back UA, `received` being that peer's
/// header lines. Per half: a DECLARED half is the more specific statement and
/// stands; an undeclared half states exactly what the peer advertised
/// ([`CapabilitySet::relayed`]), and no line where the peer advertised none.
pub fn relaying_in(
    features: Option<&FeatureActivations>,
    face: Face,
    received: &[SipHeader],
) -> CapabilitySet {
    let declared_halves = declared_advert_headers(features, face);
    let declared_set = declared_in(features, face).unwrap_or_else(CapabilitySet::silent);
    let relayed = CapabilitySet::relayed(received);
    let half = |name: HeaderName| declared_halves.contains(&name);
    CapabilitySet::stating(
        if half(HeaderName::Allow) {
            declared_set.allow().cloned()
        } else {
            relayed.allow().cloned()
        },
        if half(HeaderName::Supported) {
            declared_set.supported().cloned()
        } else {
            relayed.supported().cloned()
        },
        relayed.accept().map(<[_]>::to_vec),
    )
}

/// [`relaying_in`] for the set `call` declares.
pub fn relaying(call: &Call, face: Face, received: &[SipHeader]) -> CapabilitySet {
    relaying_in(call.features.as_ref(), face, received)
}

/// [`relaying`] on whichever face `leg_id` sits on.
pub fn relaying_for_leg(call: &Call, leg_id: &str, received: &[SipHeader]) -> CapabilitySet {
    relaying(call, Face::of_leg(leg_id), received)
}

/// The option tags the stack offers ON ITS OWN BEHALF on an INVITE it
/// originates toward a leg of `kind` (`None` reads as a destination leg):
/// under the `fake-prack` strategy the stack acknowledges a destination leg's
/// reliable provisionals itself, so that INVITE offers `100rel` (RFC 3262 §3)
/// whatever the originator advertised — an offer the originator never made is
/// not relayed back to it, and the strategy's own relay keeps its provisionals
/// unreliable. The strategy masks the provisionals of the originator's INVITE,
/// so only a leg dialled while that INVITE `awaits_final` takes the offer; a
/// leg dialled after it (a transfer target) has none to mask. A media leg's
/// provisionals belong to the service that dialled it and take no offer; every
/// other strategy offers nothing. The mint states these after every
/// advertisement and before the call-scoped withhold, so a withheld tag still
/// never rides.
pub fn offered_option_tags_in(
    features: Option<&FeatureActivations>,
    kind: Option<LegKind>,
    awaits_final: bool,
) -> Vec<String> {
    let destination = kind.unwrap_or(LegKind::Destination) == LegKind::Destination;
    let acknowledges_itself = features
        .and_then(|f| f.relay_first_18x_to_180.as_ref())
        .is_some_and(|f| f.strategy == RelayFirst18xStrategy::FakePrack);
    if destination && acknowledges_itself && awaits_final {
        vec!["100rel".to_string()]
    } else {
        Vec::new()
    }
}

/// [`offered_option_tags_in`] for the strategy `call` arms, as `call`'s
/// originator INVITE stands now.
pub fn offered_option_tags(call: &Call, kind: Option<LegKind>) -> Vec<String> {
    offered_option_tags_in(call.features.as_ref(), kind, call.a_leg.invite_final_sent.is_none())
}

/// The option tags the armed strategy WITHHOLDS from an INVITE the stack
/// originates toward a leg of `kind` (`None` reads as a destination leg),
/// `offers_sdp` being whether that INVITE carries an offer: a strategy that
/// keeps the originator's provisionals unreliable (`drop-sdp`, `keep-sdp`)
/// never relays a PRACK, so it solicits no reliable provisional (RFC 3262 §3)
/// from a destination leg — `100rel` is withheld whoever stated it, the
/// originator's relayed line or a declared set — and `fake-prack` withholds
/// it where the INVITE carries no offer, since the answer a reliable
/// provisional would then carry is one this stack cannot acknowledge. The
/// twin of [`offered_option_tags_in`]. `promote-pem-to-200` answers the
/// originator with a 200 of its own and absorbs the destination's, so the
/// destination's session interval reaches nobody who would refresh it: that
/// strategy withholds `timer` (RFC 4028 §7.1). The withhold outranks the
/// offer, and it applies on EVERY mint of the call — the initial route and each leg a
/// rule creates — so the call solicits the same reliability from each callee.
/// A media leg's provisionals belong to the service that dialled it: nothing
/// is withheld there.
pub fn withheld_by_strategy_in(
    features: Option<&FeatureActivations>,
    kind: Option<LegKind>,
    offers_sdp: bool,
) -> Vec<String> {
    let destination = kind.unwrap_or(LegKind::Destination) == LegKind::Destination;
    let withheld: &[&str] = match features.and_then(|f| f.relay_first_18x_to_180.as_ref()) {
        Some(f) => match f.strategy {
            RelayFirst18xStrategy::DropSdp | RelayFirst18xStrategy::KeepSdp => &["100rel"],
            RelayFirst18xStrategy::FakePrack if !offers_sdp => &["100rel"],
            RelayFirst18xStrategy::FakePrack => &[],
            RelayFirst18xStrategy::PromotePemTo200 => &["timer"],
        },
        None => &[],
    };
    if destination {
        withheld.iter().map(|tag| tag.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// Every option tag withheld from an INVITE `call` originates toward a leg of
/// `kind`: the call-scoped declaration (`features.withhold_option_tags`), the
/// armed strategy's own ([`withheld_by_strategy_in`]) and, toward a media leg,
/// `100rel` — no service acknowledges a media leg's reliable provisionals
/// (RFC 3262 §4) — as one set: what `build_b_leg` narrows the assembled
/// `Supported`/`Require` lines by.
pub fn withheld_option_tags(call: &Call, kind: Option<LegKind>, offers_sdp: bool) -> Vec<String> {
    withheld_option_tags_in(call.features.as_ref(), kind, offers_sdp)
}

/// [`withheld_option_tags`] for the call-scoped `features`.
pub fn withheld_option_tags_in(
    features: Option<&FeatureActivations>,
    kind: Option<LegKind>,
    offers_sdp: bool,
) -> Vec<String> {
    let mut withheld: Vec<String> =
        features.and_then(|f| f.withhold_option_tags.clone()).unwrap_or_default();
    let media = (kind == Some(LegKind::Media)).then(|| "100rel".to_string());
    for tag in withheld_by_strategy_in(features, kind, offers_sdp).into_iter().chain(media) {
        if !withheld.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
            withheld.push(tag);
        }
    }
    withheld
}

/// A final to the originator's INVITE refused in the element's own name:
/// 4xx–6xx, less the 487 that reports the request's own termination (RFC 3261
/// §9.2, §15).
pub(crate) fn is_minted_failure(status: u16) -> bool {
    (400..700).contains(&status) && status != 487
}

/// Append `set`, the deployment's advertisement for a minted final, to
/// `extra`, each half only where neither `extra` nor the decision (`stated`)
/// names it.
pub(crate) fn stamp_own_advertisement(
    set: &CapabilitySet,
    stated: impl Fn(&HeaderName) -> bool,
    extra: &mut Vec<SipHeader>,
) {
    for (name, value) in set.lines() {
        if !stated(&name) && !extra.iter().any(|h| name.matches(&h.name)) {
            extra.push(SipHeader {
                name: SipStr::owned(name.as_wire_str()),
                value: SipStr::owned(&value),
            });
        }
    }
}

/// Read the replicated token lists into the typed value the SIP layer stamps.
/// A half the declaration omits is unstated; a half it states EMPTY advertises
/// the empty set. Tokens that are not RFC 3261 §25.1 `token`s are dropped by
/// the typed set, so nothing a decision states can reach the wire as anything
/// but option tags. A declaration states no `Accept`: that half is only ever
/// relayed.
fn typed(declared: &AdvertisedCapabilities) -> CapabilitySet {
    CapabilitySet::stating(
        declared.allow.as_ref().map(|tokens| Allow::of(tokens.iter().map(String::as_str))),
        declared.supported.as_ref().map(|tokens| Supported::of(tokens.iter().map(String::as_str))),
        None,
    )
}

#[cfg(test)]
mod tests;
