//! A callee dialled after the caller's 487 (ADR-0055 §11,
//! `W-kill-reask-after-cancel`): the caller's CANCEL was answered while her
//! call had dialled nobody, and an initial INVITE reached a callee afterwards.

use std::net::SocketAddr;

use sip_message::Method;
use sip_net_testkit::{CaptureIndex, Captured, RecordedSipEntry};

/// Whether `capture` holds a 487 to `caller`'s INVITE delivered before any
/// initial INVITE reached one of `callees`, and one delivered after it.
pub(super) fn dialled_after_the_callers_487(
    capture: &CaptureIndex,
    caller: SocketAddr,
    callees: &[SocketAddr],
) -> bool {
    let dials = |e: &RecordedSipEntry, m: &Captured| {
        e.delivered
            && callees.contains(&e.to)
            && m.method() == Some(&Method::Invite)
            && m.to_tag.is_none()
    };
    let cancelled = capture.iter().find(|(_, e, m)| {
        e.delivered && e.to == caller && m.status() == Some(487) && m.cseq_method == Method::Invite
    });
    let Some((at, _, _)) = cancelled else { return false };
    let before = capture.iter().any(|(i, e, m)| i < at && dials(e, m));
    let after = capture.iter().any(|(i, e, m)| i > at && dials(e, m));
    !before && after
}

#[cfg(test)]
mod tests;
