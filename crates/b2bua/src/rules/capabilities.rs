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
mod tests {
    use super::*;
    use call::features::{
        AdvertiseCapabilitiesFeature, KeepaliveActivation, PlatformActivations, Relay18xMessages,
        RelayFirst18xTo180Feature,
    };

    /// Feature activations declaring `caps` toward the originated face only.
    fn declaring(caps: AdvertisedCapabilities) -> FeatureActivations {
        FeatureActivations {
            platform: PlatformActivations {
                max_duration_sec: 3_600,
                max_duration_anchor: Default::default(),
                keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            charging_vector: None,
            withhold_option_tags: None,
            stated_headers: None,
            uncharged_media_legs: false,
            withhold_on_relayed_provisionals: None,
            advertise_capabilities: Some(AdvertiseCapabilitiesFeature {
                toward_originator: None,
                toward_originated: Some(caps),
            }),
        }
    }

    fn originated(features: &FeatureActivations) -> CapabilitySet {
        declared_in(Some(features), Face::Originated).expect("the face is declared")
    }

    /// The motivating narrowing — "the same methods, minus REFER and INFO" —
    /// states `allow` alone and leaves the option tags unstated: nothing of
    /// the stack's own is filled in beside a declaration.
    #[test]
    fn declaring_the_method_half_alone_leaves_the_option_tags_unstated() {
        let features = declaring(AdvertisedCapabilities {
            allow: Some(vec!["INVITE".into(), "ACK".into(), "CANCEL".into(), "BYE".into()]),
            supported: None,
        });
        let caps = originated(&features);
        assert_eq!(caps.allow_text().as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
        assert_eq!(caps.supported(), None);
        assert_eq!(declared_advert_headers(Some(&features), Face::Originated), [HeaderName::Allow]);
    }

    /// …and the mirror: option tags alone leave the methods unstated.
    #[test]
    fn declaring_the_option_tag_half_alone_leaves_the_methods_unstated() {
        let features = declaring(AdvertisedCapabilities {
            allow: None,
            supported: Some(vec!["timer".into()]),
        });
        let caps = originated(&features);
        assert_eq!(caps.allow(), None);
        assert_eq!(caps.supported_text().as_deref(), Some("timer"));
        assert_eq!(
            declared_advert_headers(Some(&features), Face::Originated),
            [HeaderName::Supported]
        );
    }

    /// An EMPTY half states the empty set (a value-less line); an ABSENT half
    /// states no line — two different statements.
    #[test]
    fn an_empty_half_states_the_empty_set_where_an_absent_half_states_no_line() {
        let empty = declaring(AdvertisedCapabilities {
            allow: Some(Vec::new()),
            supported: Some(Vec::new()),
        });
        let caps = originated(&empty);
        assert_eq!(caps.allow_text().as_deref(), Some(""));
        assert_eq!(caps.supported_text().as_deref(), Some(""));

        let absent = declaring(AdvertisedCapabilities { allow: None, supported: None });
        assert_eq!(originated(&absent), CapabilitySet::silent());
    }

    /// A declaration a decision supplied is still read through the token
    /// grammar: an entry carrying CRLF cannot reach a mint point.
    #[test]
    fn a_declared_token_that_is_not_a_token_is_dropped_before_any_mint_point() {
        let features = declaring(AdvertisedCapabilities {
            allow: Some(vec!["INVITE".into(), "ACK\r\nX-Evil: injected".into()]),
            supported: None,
        });
        assert_eq!(originated(&features).allow_text().as_deref(), Some("INVITE"));
    }

    /// Nothing declared anywhere: an undeclared face states NO line on a
    /// message it mints with nothing to relay, and exactly the peer's lines
    /// on one that carries the peer's advertisement.
    #[test]
    fn an_undeclared_face_states_nothing_of_its_own() {
        let features = declaring(AdvertisedCapabilities { allow: None, supported: None });
        assert_eq!(declared_in(Some(&features), Face::Originator), None);
        assert_eq!(declared_in(None, Face::Originated), None);
        assert!(declared_advert_headers(None, Face::Originated).is_empty());
        assert_eq!(relaying_in(None, Face::Originator, &[]), CapabilitySet::silent());
        let peer = vec![
            SipHeader { name: "Allow".into(), value: "INVITE, ACK, BYE".into() },
            SipHeader { name: "Accept".into(), value: "application/sdp, application/isup".into() },
        ];
        let relayed = relaying_in(None, Face::Originator, &peer);
        assert_eq!(relayed.allow_text().as_deref(), Some("INVITE, ACK, BYE"));
        assert_eq!(relayed.supported(), None);
        assert_eq!(relayed.accept_text().as_deref(), Some("application/sdp, application/isup"));
    }

    /// A declared half stands over the peer's; the undeclared halves are the
    /// peer's verbatim, `Accept` included.
    #[test]
    fn a_declared_half_stands_over_the_relayed_one() {
        let features = declaring(AdvertisedCapabilities {
            allow: Some(vec!["INVITE".into(), "ACK".into(), "BYE".into()]),
            supported: None,
        });
        let peer = vec![
            SipHeader { name: "Allow".into(), value: "INVITE, ACK, BYE, REFER".into() },
            SipHeader { name: "Supported".into(), value: "timer".into() },
        ];
        let caps = relaying_in(Some(&features), Face::Originated, &peer);
        assert_eq!(caps.allow_text().as_deref(), Some("INVITE, ACK, BYE"));
        assert_eq!(caps.supported_text().as_deref(), Some("timer"));
        assert_eq!(caps.accept(), None);
    }

    /// Feature activations arming `strategy` and declaring nothing.
    fn arming(strategy: RelayFirst18xStrategy) -> FeatureActivations {
        FeatureActivations {
            advertise_capabilities: None,
            relay_first_18x_to_180: Some(RelayFirst18xTo180Feature {
                strategy,
                messages: Relay18xMessages::First,
            }),
            ..declaring(AdvertisedCapabilities { allow: None, supported: None })
        }
    }

    /// Under `fake-prack` a destination leg — stated or defaulted — is offered
    /// `100rel` on the stack's own behalf; a media leg is offered nothing.
    #[test]
    fn fake_prack_offers_100rel_to_a_destination_leg_and_nothing_to_a_media_leg() {
        let features = arming(RelayFirst18xStrategy::FakePrack);
        assert_eq!(offered_option_tags_in(Some(&features), None, true), ["100rel"]);
        assert_eq!(
            offered_option_tags_in(Some(&features), Some(LegKind::Destination), true),
            ["100rel"]
        );
        assert!(offered_option_tags_in(Some(&features), Some(LegKind::Media), true).is_empty());
    }

    /// No service acknowledges a media leg's reliable provisionals, so `100rel`
    /// is withheld from every INVITE toward one (RFC 3262 §4: a reliable 18x
    /// unacknowledged goes unanswered), whatever the strategy, the transparent
    /// one included; a destination leg keeps the strategy's own withhold.
    #[test]
    fn a_media_leg_is_never_offered_100rel() {
        for features in [
            None,
            Some(arming(RelayFirst18xStrategy::FakePrack)),
            Some(arming(RelayFirst18xStrategy::DropSdp)),
            Some(arming(RelayFirst18xStrategy::PromotePemTo200)),
        ] {
            let withheld = withheld_option_tags_in(features.as_ref(), Some(LegKind::Media), true);
            assert!(
                withheld.iter().any(|t| t == "100rel"),
                "100rel withheld from a media leg under {features:?}"
            );
            assert!(!withheld.iter().any(|t| t == "timer"), "only 100rel: {withheld:?}");
        }
        assert!(withheld_option_tags_in(None, Some(LegKind::Destination), true).is_empty());
        assert!(withheld_option_tags_in(None, None, true).is_empty());
    }

    /// A destination leg dialled after the originator's INVITE took its final
    /// has no provisional for the strategy to mask: it is offered nothing.
    #[test]
    fn fake_prack_offers_nothing_once_the_originator_invite_has_its_final() {
        let features = arming(RelayFirst18xStrategy::FakePrack);
        assert!(offered_option_tags_in(Some(&features), None, false).is_empty());
        assert!(
            offered_option_tags_in(Some(&features), Some(LegKind::Destination), false).is_empty()
        );
    }

    /// Any other strategy, and no strategy, offer nothing: the stack
    /// acknowledges no provisional itself, so it solicits none.
    #[test]
    fn other_strategies_offer_nothing() {
        for features in [
            None,
            Some(arming(RelayFirst18xStrategy::DropSdp)),
            Some(arming(RelayFirst18xStrategy::KeepSdp)),
            Some(arming(RelayFirst18xStrategy::PromotePemTo200)),
        ] {
            assert!(offered_option_tags_in(features.as_ref(), None, true).is_empty());
        }
    }

    /// A strategy that keeps the originator unreliable withholds `100rel`
    /// from a destination leg whether or not the INVITE carries an offer, and
    /// from a media leg never.
    #[test]
    fn an_unreliable_relay_withholds_100rel_from_a_destination_leg() {
        for strategy in [RelayFirst18xStrategy::DropSdp, RelayFirst18xStrategy::KeepSdp] {
            let features = arming(strategy);
            for offers_sdp in [true, false] {
                assert_eq!(withheld_by_strategy_in(Some(&features), None, offers_sdp), ["100rel"]);
                assert_eq!(
                    withheld_by_strategy_in(
                        Some(&features),
                        Some(LegKind::Destination),
                        offers_sdp
                    ),
                    ["100rel"]
                );
                assert!(withheld_by_strategy_in(Some(&features), Some(LegKind::Media), offers_sdp)
                    .is_empty());
            }
        }
    }

    /// `fake-prack` withholds `100rel` only where the INVITE carries no offer:
    /// with one the stack acknowledges the reliable provisional itself.
    #[test]
    fn fake_prack_withholds_100rel_only_without_an_offer() {
        let features = arming(RelayFirst18xStrategy::FakePrack);
        assert!(withheld_by_strategy_in(Some(&features), None, true).is_empty());
        assert_eq!(withheld_by_strategy_in(Some(&features), None, false), ["100rel"]);
    }

    /// No strategy at all withholds nothing.
    #[test]
    fn no_strategy_withholds_nothing() {
        for offers_sdp in [true, false] {
            assert!(withheld_by_strategy_in(None, None, offers_sdp).is_empty());
        }
    }

    /// The PEM promotion absorbs the destination's 2xx, so it withholds the
    /// session timer from a destination leg (nobody would refresh it) and
    /// nothing from a media leg.
    #[test]
    fn the_pem_promotion_withholds_the_session_timer() {
        let features = arming(RelayFirst18xStrategy::PromotePemTo200);
        for offers_sdp in [true, false] {
            assert_eq!(withheld_by_strategy_in(Some(&features), None, offers_sdp), ["timer"]);
            assert!(withheld_by_strategy_in(Some(&features), Some(LegKind::Media), offers_sdp)
                .is_empty());
        }
    }
}
