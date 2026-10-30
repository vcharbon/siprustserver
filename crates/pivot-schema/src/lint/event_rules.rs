//! Lane events (§6.7g): a `send` may wait on an event the lane observes off the
//! SIP wire. The token is the lane's, so lint checks only where it sits.

use crate::flow::Op;
use crate::lint::{at, Index, Report};

pub(super) fn check(index: &Index<'_>, report: &mut Report) {
    for (_, step) in index.all_steps() {
        let Some(event) = &step.awaits else { continue };
        let path = at("flow", &step.id);
        if step.op == Op::Expect {
            report.error(
                "awaits/on-expect",
                &path,
                "an `expect` states `awaits`",
                "an arrival waits on nothing the document can order; move the event to the send it releases",
            );
        }
        if event.trim().is_empty() {
            report.error(
                "awaits/empty",
                &path,
                "`awaits` names no event",
                "name a token of the lane's event registry, e.g. `decision-request-in-flight`",
            );
        }
    }
}
