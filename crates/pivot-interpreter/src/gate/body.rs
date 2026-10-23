//! What an expect states about the body it takes (`PCAP2TEST_PIVOT_V3.md`
//! §8.3): the presence and container type an asserted expect gates its MATCH
//! on, and the body TYPE every expect holds the arrival to.
//!
//! The TYPE is the body's presence, its bare media type (`type/subtype`,
//! parameters such as `boundary` ignored) and, for a decomposed multipart
//! expectation, the part count and each part's bare media type by position.
//! The multipart framing — boundary, delimiter layout, preamble — and a part's
//! entity headers other than `Content-Type` are never part of it. Content
//! stays the post-run confrontation's under `check: record`.

use pivot_schema::body::{Body, BodyCompare, BodyShape};
use pivot_schema::bundle::Failure;
use pivot_schema::known_bug::KnownBug;
use pivot_schema::msg::MsgSpec;
use sip_message::header::{HeaderValue, MediaType};
use sip_message::{entity_parts, SipStr};

use super::Inbound;
use crate::plan::CompiledStep;
use crate::scope::{Finding, Scope};

/// What a body type that did not hold costs the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyTypeFinding {
    /// The miss fails the run: unclassified, it gates on every lane.
    Gating(Finding),
    /// The miss is the one this lane's declared known bug names: recorded,
    /// never failing.
    Waived(KnownBug, Failure),
}

/// The body type `step` states, held against the datagram the step took,
/// under `check: assert` and `check: record` alike. `None` where the step
/// states no body or the type held.
///
/// A step keeps the datagram whatever this finds: the call walks to its
/// teardown and the finding fails the run, never the script. Under `assert`
/// the presence and container type already gated the match ([`body_holds`]),
/// so what is left there is a resource's media type and a multipart's parts.
pub fn body_type_finding(
    step: &CompiledStep,
    inbound: &Inbound,
    scope: &Scope<'_>,
) -> Option<BodyTypeFinding> {
    if !step.is_expect() {
        return None;
    }
    let body = step.msg.body.as_ref()?;
    let arrived = ArrivedType::of(inbound);
    if type_holds(body, &arrived) {
        return None;
    }
    let failure = Failure::CheckFailed {
        site: format!("step {:?}", step.id),
        field: "body".into(),
        op: "type".into(),
        expected: stated_type(body),
        observed: arrived.to_string(),
    };
    let bug = match (body, &arrived) {
        (Body::Shape(shape), ArrivedType::Typed { .. }) if shape.mode == BodyShape::Absent => {
            unstripped_provisional(&step.msg)
        }
        _ => None,
    };
    Some(match bug.filter(|bug| scope.waives(*bug)) {
        Some(bug) => BodyTypeFinding::Waived(bug, failure),
        None => BodyTypeFinding::Gating(Finding::gating(failure)),
    })
}

/// The body type a datagram carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ArrivedType {
    /// No body bytes, whatever the headers state.
    Absent,
    /// A body, under its bare media type; `parts` holds each part's bare media
    /// type in wire order where the type is `multipart/…`.
    Typed { media: String, parts: Option<Vec<String>> },
}

impl ArrivedType {
    fn of(inbound: &Inbound) -> ArrivedType {
        if inbound.body.is_empty() {
            return ArrivedType::Absent;
        }
        let Some(content_type) = inbound.content_type.as_deref() else {
            return ArrivedType::Typed { media: "untyped".into(), parts: None };
        };
        let media = bare(content_type);
        let parts = media.starts_with("multipart/").then(|| {
            entity_parts(Some(content_type), &[], &inbound.body)
                .iter()
                .map(|part| bare(&part.content_type))
                .collect()
        });
        ArrivedType::Typed { media, parts }
    }
}

impl std::fmt::Display for ArrivedType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArrivedType::Absent => f.write_str("absent"),
            ArrivedType::Typed { media, parts: None } => f.write_str(media),
            ArrivedType::Typed { media, parts: Some(parts) } => {
                write!(f, "{media} ({})", parts.join(", "))
            }
        }
    }
}

/// Whether what arrived is of the type `body` states.
fn type_holds(body: &Body, arrived: &ArrivedType) -> bool {
    let ArrivedType::Typed { media, parts } = arrived else {
        return matches!(body, Body::Shape(shape) if shape.mode == BodyShape::Absent);
    };
    match body {
        Body::Shape(shape) => match shape.mode {
            BodyShape::Absent => false,
            BodyShape::SdpPresent => media == "application/sdp",
            BodyShape::MultipartPresent => media.starts_with("multipart/"),
        },
        Body::Resource(resource) => *media == resource_type(resource.content_type.as_deref()),
        Body::Multipart(multipart) => {
            *media == bare(&multipart.multipart.content_type)
                && parts.as_ref().is_some_and(|parts| {
                    parts.len() == multipart.multipart.parts.len()
                        && parts
                            .iter()
                            .zip(&multipart.multipart.parts)
                            .all(|(got, want)| *got == bare(&want.content_type))
                })
        }
    }
}

/// The type `body` states, as a finding names it: `absent`, a bare media
/// type, `multipart/*` for the shape, or a container with its parts' types.
fn stated_type(body: &Body) -> String {
    match body {
        Body::Shape(shape) => match shape.mode {
            BodyShape::Absent => "absent".into(),
            BodyShape::SdpPresent => "application/sdp".into(),
            BodyShape::MultipartPresent => "multipart/*".into(),
        },
        Body::Resource(resource) => resource_type(resource.content_type.as_deref()),
        Body::Multipart(multipart) => {
            let parts: Vec<String> =
                multipart.multipart.parts.iter().map(|part| bare(&part.content_type)).collect();
            format!("{} ({})", bare(&multipart.multipart.content_type), parts.join(", "))
        }
    }
}

/// A resource's bare media type: its stated `content-type`, else bare
/// `application/sdp`, the one type a resource leaves unstated.
fn resource_type(content_type: Option<&str>) -> String {
    content_type.map_or_else(|| "application/sdp".into(), bare)
}

/// The bare `type/subtype` of a media type, lowercased, parameters stripped;
/// a value that does not parse as one is kept as it reads.
fn bare(content_type: &str) -> String {
    MediaType::parse(&SipStr::owned(content_type))
        .map(|media| media.token().to_ascii_lowercase())
        .unwrap_or_else(|_| content_type.trim().to_ascii_lowercase())
}

/// The body an ASSERTED expect gates its match on, and the lane-declarable
/// known bug that names the miss where one does: presence, an `sdp-present`
/// shape or a resource compared as `sdp` arriving as SDP, and a multipart
/// expectation arriving as `multipart/…`. A resource's other media types and a
/// multipart's parts are [`body_type_finding`]'s, read after the match.
///
/// The one nameable miss is a body on a PROVISIONAL the document expects
/// stripped: that is a SUT that did not apply the 18x rewrite its routing
/// decision armed, and the call continues around it. A body missing where one
/// is owed, or the wrong media type, names no known bug and always gates.
pub(super) fn body_holds(spec: &MsgSpec, inbound: &Inbound) -> Option<(String, Option<KnownBug>)> {
    let Some(body) = &spec.body else { return None };
    let media =
        inbound.content_type.as_deref().and_then(|t| MediaType::parse(&SipStr::owned(t)).ok());
    let is_sdp = media.as_ref().is_some_and(MediaType::is_sdp);
    let is_multipart = media.as_ref().is_some_and(MediaType::is_multipart);
    let content_type = || inbound.content_type.as_deref().unwrap_or("absent").to_string();
    match body {
        Body::Shape(shape) => match shape.mode {
            BodyShape::Absent if !inbound.body.is_empty() => Some((
                format!("body must be absent; {} bytes arrived", inbound.body.len()),
                unstripped_provisional(spec),
            )),
            BodyShape::SdpPresent if !is_sdp => {
                Some((format!("body must be SDP; content type is {:?}", content_type()), None))
            }
            BodyShape::MultipartPresent if !is_multipart => Some((
                format!("body must be multipart; content type is {:?}", content_type()),
                None,
            )),
            _ => None,
        },
        Body::Resource(_) | Body::Multipart(_) if inbound.body.is_empty() => {
            Some(("body must be present; none arrived".into(), None))
        }
        Body::Resource(resource) if resource.compare == Some(BodyCompare::Sdp) && !is_sdp => {
            Some((format!("body must be SDP; content type is {:?}", content_type()), None))
        }
        Body::Multipart(_) if !is_multipart => {
            Some((format!("body must be multipart; content type is {:?}", content_type()), None))
        }
        Body::Resource(_) | Body::Multipart(_) => None,
    }
}

/// The known bug a body-on-a-stripped-provisional miss falls under, where the
/// step is one: a 1xx above 100, which is the message a `relay18x` mode
/// rewrites. On any other message an unexpected body is nobody's known defect.
fn unstripped_provisional(spec: &MsgSpec) -> Option<KnownBug> {
    matches!(spec.status, Some(101..=199)).then_some(KnownBug::ProvisionalRewriteNotApplied)
}

#[cfg(test)]
mod tests;
