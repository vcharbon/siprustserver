//! Which capability set the B2BUA advertises on each face of a call.
//!
//! The advertisement is call-scoped policy: the decision engine declares it in
//! `features.advertise_capabilities`, the declaration replicates with the call,
//! and every mint point reads the face it is emitting on through this module.
//!
//! Precedence at every mint point, most specific first: an explicit header
//! update carried on the message (a decision's `header_updates`, a firing
//! rule's own `Allow`/`Supported`) beats the call's declared set for that face,
//! which beats [`CapabilitySet::default`] — the stack set, which is what an
//! undeclared face advertises. A value RELAYED from the other peer is not an
//! explicit update and never outranks a declaration: the messages that carry
//! one, the relayed requests, drop it (see [`declared_advert_headers`]).
//!
//! The two halves resolve independently — an undeclared `Allow` keeps the stack
//! methods while a declared `Supported` narrows the option tags — and an empty
//! declared half advertises the empty set rather than falling back.
//!
//! A DECLARED set is read through the token grammar, so nothing it states can
//! reach the wire as anything but option tags. An explicit `header_updates`
//! line is not: it states its own bytes verbatim, and the decision layer owns
//! their well-formedness.

use call::features::{AdvertisedCapabilities, FeatureActivations};
use call::Call;
use sip_message::generators::CapabilitySet;
use sip_message::header::{Allow, HeaderName, Supported};

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
/// when nothing is declared. An undeclared HALF of a declared face resolves to
/// the stack's value for that half, so a caller narrows the methods without
/// pinning a copy of the stack's option tags.
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
    let Some(declared) = declared_face(features, face) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if declared.allow.is_some() {
        names.push(HeaderName::Allow);
    }
    if declared.supported.is_some() {
        names.push(HeaderName::Supported);
    }
    names
}

/// The face's declaration inside `features`, if any.
fn declared_face(
    features: Option<&FeatureActivations>,
    face: Face,
) -> Option<&AdvertisedCapabilities> {
    let arm = features?.advertise_capabilities.as_ref()?;
    match face {
        Face::Originator => arm.toward_originator.as_ref(),
        Face::Originated => arm.toward_originated.as_ref(),
    }
}

/// The set the call DECLARED for `face`, or `None` when it declared none.
pub fn declared(call: &Call, face: Face) -> Option<CapabilitySet> {
    declared_in(call.features.as_ref(), face)
}

/// The set advertised on `face`: the declared one, else the stack default.
pub fn advertised(call: &Call, face: Face) -> CapabilitySet {
    declared(call, face).unwrap_or_default()
}

/// The set advertised on whichever face `leg_id` sits on.
pub fn for_leg(call: &Call, leg_id: &str) -> CapabilitySet {
    advertised(call, Face::of_leg(leg_id))
}

/// Read the replicated token lists into the typed value the SIP layer stamps.
/// A half the declaration omits keeps the stack's value for that half; a half
/// it states EMPTY advertises the empty set. Tokens that are not RFC 3261
/// §25.1 `token`s are dropped by the typed set, so nothing a decision states
/// can reach the wire as anything but option tags.
fn typed(declared: &AdvertisedCapabilities) -> CapabilitySet {
    let stack = CapabilitySet::default();
    let allow = match &declared.allow {
        Some(tokens) => Allow::of(tokens.iter().map(String::as_str)),
        None => stack.allow().clone(),
    };
    let supported = match &declared.supported {
        Some(tokens) => Supported::of(tokens.iter().map(String::as_str)),
        None => stack.supported().clone(),
    };
    CapabilitySet::new(allow, supported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use call::features::{
        AdvertiseCapabilitiesFeature, KeepaliveActivation, PlatformActivations,
    };

    /// Feature activations declaring `caps` toward the originated face only.
    fn declaring(caps: AdvertisedCapabilities) -> FeatureActivations {
        FeatureActivations {
            platform: PlatformActivations {
                max_duration_sec: 3_600,
                keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            call_limiters: None,
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
    /// states `allow` alone and keeps the stack's option tags, so a caller
    /// never freezes a copy of `B2BUA_SUPPORTED` in its configuration.
    #[test]
    fn declaring_the_method_half_alone_keeps_the_stack_option_tags() {
        let features = declaring(AdvertisedCapabilities {
            allow: Some(vec!["INVITE".into(), "ACK".into(), "CANCEL".into(), "BYE".into()]),
            supported: None,
        });
        let caps = originated(&features);
        assert_eq!(caps.allow_text(), "INVITE, ACK, CANCEL, BYE");
        assert_eq!(caps.supported_text(), CapabilitySet::default().supported_text());
        assert_eq!(declared_advert_headers(Some(&features), Face::Originated), [HeaderName::Allow]);
    }

    /// …and the mirror: option tags alone leave the methods at the stack set.
    #[test]
    fn declaring_the_option_tag_half_alone_keeps_the_stack_methods() {
        let features =
            declaring(AdvertisedCapabilities { allow: None, supported: Some(vec!["timer".into()]) });
        let caps = originated(&features);
        assert_eq!(caps.allow_text(), CapabilitySet::default().allow_text());
        assert_eq!(caps.supported_text(), "timer");
        assert_eq!(
            declared_advert_headers(Some(&features), Face::Originated),
            [HeaderName::Supported]
        );
    }

    /// An EMPTY half is "advertise nothing", NOT "fall back to the stack" —
    /// the two are different statements and resolve differently.
    #[test]
    fn an_empty_half_advertises_nothing_where_an_absent_half_falls_back() {
        let empty = declaring(AdvertisedCapabilities {
            allow: Some(Vec::new()),
            supported: Some(Vec::new()),
        });
        let caps = originated(&empty);
        assert_eq!(caps.allow_text(), "");
        assert_eq!(caps.supported_text(), "");

        let absent = declaring(AdvertisedCapabilities { allow: None, supported: None });
        assert_eq!(originated(&absent), CapabilitySet::default());
    }

    /// A declaration a decision supplied is still read through the token
    /// grammar: an entry carrying CRLF cannot reach a mint point.
    #[test]
    fn a_declared_token_that_is_not_a_token_is_dropped_before_any_mint_point() {
        let features = declaring(AdvertisedCapabilities {
            allow: Some(vec!["INVITE".into(), "ACK\r\nX-Evil: injected".into()]),
            supported: None,
        });
        assert_eq!(originated(&features).allow_text(), "INVITE");
    }

    /// Nothing declared anywhere: every face resolves to the stack set.
    #[test]
    fn an_undeclared_face_resolves_to_the_stack_set() {
        let features = declaring(AdvertisedCapabilities { allow: None, supported: None });
        assert_eq!(declared_in(Some(&features), Face::Originator), None);
        assert_eq!(declared_in(None, Face::Originated), None);
        assert!(declared_advert_headers(None, Face::Originated).is_empty());
    }
}
