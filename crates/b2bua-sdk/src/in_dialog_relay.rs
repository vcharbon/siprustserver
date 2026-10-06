//! What an in-dialog request carries across the back-to-back UA (RFC 3261
//! §16.6): the one rule the core's relay and a service relaying a request it
//! held both apply, so a request relayed later reads as one relayed at once.

use call::features::{AdvertisedCapabilities, FeatureActivations};
use sip_message::generators::{self, RelayScope};
use sip_message::header::HeaderName;
use sip_message::{SipHeader, SipRequest};

/// The advertisement `features` declare for the face toward the originator
/// (`toward_originator`) or toward a leg the B2BUA originated, if any.
pub fn declared_face(
    features: Option<&FeatureActivations>,
    toward_originator: bool,
) -> Option<&AdvertisedCapabilities> {
    let arm = features?.advertise_capabilities.as_ref()?;
    if toward_originator {
        arm.toward_originator.as_ref()
    } else {
        arm.toward_originated.as_ref()
    }
}

/// The advertisement headers [`declared_face`] states for itself: the halves a
/// relayed request leaves the peer's line of behind, so the declared narrowing
/// is never undone by a copy. Empty where the face declares nothing.
pub fn declared_halves(
    features: Option<&FeatureActivations>,
    toward_originator: bool,
) -> Vec<HeaderName> {
    let Some(declared) = declared_face(features, toward_originator) else {
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

/// The lines of the received in-dialog request `req` that ride its relay: every
/// header it carries that this stack does not own, under `scope`
/// ([`generators::relayable_request_headers`]), but the advertisement halves
/// `declared` for the target face and the advertisement headers RFC 3261 §20
/// Tables 2 and 3 mark not applicable to the method ([`generators::admitted_on`]).
/// A relayed REFER keeps its `Refer-To` / `Referred-By` and a relayed NOTIFY its
/// `Event` / `Subscription-State`: the generator states those only for a NOTIFY
/// this stack originates. The received `RAck` stays behind: the generator states
/// the target leg's (RFC 3262 §7.2). A re-INVITE or UPDATE keeps the
/// session-timer demand of its `Require` (RFC 4028 §7.1).
pub fn relayed_request_lines(
    req: &SipRequest,
    declared: &[HeaderName],
    scope: RelayScope<'_>,
) -> Vec<SipHeader> {
    let mut lines = generators::relayable_request_headers(req, scope);
    lines.retain(|h| {
        !declared.iter().any(|name| name.matches(&h.name))
            && generators::admitted_on(h.name.as_str(), req.method())
    });
    lines
}
