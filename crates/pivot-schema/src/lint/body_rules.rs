//! Body well-formedness: what a stored body may state given the step's `op`
//! and its content type.
//!
//! A resource body on an EXPECT asserts the received body against the file
//! (§8.3), and `compare` says how the two are held together. On a SEND the
//! file is what goes on the wire and there is nothing to compare it with, so a
//! `compare` there describes a check nobody runs and is refused rather than
//! ignored. The `sdp` compare reads a session description, so a resource that
//! states another content type under it names a fold that cannot read the
//! file.

use crate::body::{Body, BodyCompare};
use crate::flow::Op;
use crate::lint::{at, Index, Report};

/// The bare `type/subtype` of a content type, lowercased, parameters stripped.
fn mime_head(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for (_, step) in index.all_steps() {
        let Some(Body::Resource(resource)) = &step.msg.body else { continue };
        if step.op == Op::Send && resource.compare.is_some() {
            report.error(
                "body/compare-on-send",
                at("flow", &step.id),
                "a send states `body.compare`, which only an expect reads",
                "drop `compare`: the send emits the resource and compares it with nothing",
            );
        }
        if resource.compare == Some(BodyCompare::Sdp)
            && resource.content_type.as_deref().is_some_and(|t| mime_head(t) != "application/sdp")
        {
            report.error(
                "body/compare-sdp-type",
                at("flow", &step.id),
                format!(
                    "`compare: sdp` on a body whose content type is {:?}, which is not a session description",
                    resource.content_type.as_deref().unwrap_or_default()
                ),
                "compare an `application/sdp` body as `sdp`; any other type compares `exact` or `xml`",
            );
        }
    }
}
