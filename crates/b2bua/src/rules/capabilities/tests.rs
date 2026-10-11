use super::*;
use call::features::{
    AdvertiseCapabilitiesFeature, KeepaliveActivation, PlatformActivations, Relay18xMessages,
    RelayFirst18xTo180Feature,
};

/// Feature activations declaring `caps` toward the originated face only.
fn declaring(caps: AdvertisedCapabilities) -> FeatureActivations {
    FeatureActivations {
        platform: PlatformActivations {
            max_duration_ms: 3_600_000,
            max_duration_anchor: Default::default(),
            keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
        },
        refer: None,
        relay_first_18x_to_180: None,
        no_answer_ms: None,
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
    let features =
        declaring(AdvertisedCapabilities { allow: None, supported: Some(vec!["timer".into()]) });
    let caps = originated(&features);
    assert_eq!(caps.allow(), None);
    assert_eq!(caps.supported_text().as_deref(), Some("timer"));
    assert_eq!(declared_advert_headers(Some(&features), Face::Originated), [HeaderName::Supported]);
}

/// An EMPTY half states the empty set (a value-less line); an ABSENT half
/// states no line — two different statements.
#[test]
fn an_empty_half_states_the_empty_set_where_an_absent_half_states_no_line() {
    let empty =
        declaring(AdvertisedCapabilities { allow: Some(Vec::new()), supported: Some(Vec::new()) });
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
    assert!(offered_option_tags_in(Some(&features), Some(LegKind::Destination), false).is_empty());
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
                withheld_by_strategy_in(Some(&features), Some(LegKind::Destination), offers_sdp),
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
        assert!(
            withheld_by_strategy_in(Some(&features), Some(LegKind::Media), offers_sdp).is_empty()
        );
    }
}
