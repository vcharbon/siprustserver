//! What the promotion advertises toward Alice: the same set every other
//! mint point of the call resolves — declared, else Bob's own relayed
//! advertisement, else the stack set — minus `100rel` and `timer`, which
//! this service never claims.
use super::*;
use call::features::{
    AdvertiseCapabilitiesFeature, AdvertisedCapabilities, KeepaliveActivation, PlatformActivations,
};
use sip_message::SipStr;

fn received(lines: &[(&str, &str)]) -> Vec<SipHeader> {
    lines
        .iter()
        .map(|(name, value)| SipHeader { name: SipStr::owned(name), value: SipStr::owned(value) })
        .collect()
}

/// The `(Allow, Supported)` values the advert stamps, as they reach Alice;
/// `None` where the advert carries no such line.
fn advert_relaying(
    features: Option<&FeatureActivations>,
    from_bob: &[SipHeader],
) -> (Option<String>, Option<String>) {
    let entries = a_facing_advert(features, from_bob);
    let text = |name: HeaderName| {
        entries.iter().find(|e| e.is(&name)).map(|e| e.text().as_str().to_string())
    };
    (text(HeaderName::Allow), text(HeaderName::Supported))
}

fn features_declaring(allow: &[&str], supported: &[&str]) -> FeatureActivations {
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
            toward_originator: Some(AdvertisedCapabilities {
                allow: Some(allow.iter().map(|s| s.to_string()).collect()),
                supported: Some(supported.iter().map(|s| s.to_string()).collect()),
            }),
            toward_originated: None,
        }),
    }
}

fn advert(features: Option<&FeatureActivations>) -> (Option<String>, Option<String>) {
    advert_relaying(features, &[])
}

/// Nothing declared and nothing advertised by Bob: no line at all. The
/// service claims no set of its own, exactly like every other mint point.
#[test]
fn an_undeclared_call_with_a_silent_bob_advertises_nothing() {
    assert_eq!(advert(None), (None, None));
}

/// Bob's own advertisement travels to Alice verbatim, so the promoted 200
/// states what the generic relay would have stated — `100rel` and `timer`
/// excepted.
#[test]
fn bobs_advertisement_reaches_alice_but_never_100rel_or_timer() {
    let (allow, supported) = advert_relaying(
        None,
        &received(&[("Allow", "INVITE, ACK, BYE"), ("Supported", "100rel, timer, foo")]),
    );
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, BYE"));
    assert_eq!(supported.as_deref(), Some("foo"));
}

/// The advertised set does not depend on which rule minted the message:
/// the service resolves what every other mint point resolves, and the one
/// deliberate difference is the two tags it may not claim.
#[test]
fn the_service_advertises_what_every_other_mint_point_resolves() {
    let from_bob = received(&[("Allow", "INVITE, ACK, BYE"), ("Supported", "100rel, timer")]);
    let generic = capabilities::relaying_in(None, Face::Originator, &from_bob);
    let (allow, supported) = advert_relaying(None, &from_bob);
    assert_eq!(allow, generic.allow_text());
    assert_eq!(
        supported,
        generic.without_option_tag("100rel").without_option_tag("timer").supported_text()
    );
}

/// The declaration wins — and `100rel` and `timer` are dropped from it even
/// when the call declares them: Alice saw no reliable provisional from us,
/// and the promoted 200 states no session interval.
#[test]
fn a_declared_set_wins_and_never_claims_100rel_or_timer() {
    let features =
        features_declaring(&["INVITE", "ACK", "CANCEL", "BYE"], &["100rel", "timer", "replaces"]);
    let (allow, supported) = advert_relaying(Some(&features), &received(&[("Allow", "MESSAGE")]));
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
    assert_eq!(supported.as_deref(), Some("replaces"));
}
