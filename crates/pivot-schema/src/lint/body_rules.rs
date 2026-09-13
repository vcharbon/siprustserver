//! Body well-formedness: what a stored body may state given the step's `op`.
//!
//! A resource body on an EXPECT asserts the received body against the file
//! (§8.3), and `compare` says how the two are held together. On a SEND the
//! file is what goes on the wire and there is nothing to compare it with, so a
//! `compare` there describes a check nobody runs and is refused rather than
//! ignored.

use crate::body::Body;
use crate::flow::Op;
use crate::lint::{at, Index, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for (_, step) in index.all_steps() {
        if step.op != Op::Send {
            continue;
        }
        let Some(Body::Resource(resource)) = &step.msg.body else { continue };
        if resource.compare.is_some() {
            report.error(
                "body/compare-on-send",
                at("flow", &step.id),
                "a send states `body.compare`, which only an expect reads",
                "drop `compare`: the send emits the resource and compares it with nothing",
            );
        }
    }
}
