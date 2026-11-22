//! What the recording of a lossy call shows of an in-dialog request a later
//! one overtook: the network lost its first copy, its sender sent the next
//! request of the dialog after it, and its receiver refused the copy that
//! reached it 500, its CSeq gone backwards (RFC 3261 §12.2.2, §17.1.2.2).
//! The 500 is the receiver's right answer, so the call's scenario may fail on
//! that request's transaction, or on its direct consequence on the request's
//! leg, each read from the wire ([`excuses`]); any other failure stays one,
//! and every other net still judges the call.

use std::net::SocketAddr;

use layer_harness::{LaneKey, Stamped};
use scenario_harness::actor::FailedAt;
use scenario_harness::StepError;
use sip_message::sniff::{self, Identity};
use sip_net_testkit::SignalingNetworkEvent;

/// A request a later one overtook after the loss of its first copy, refused
/// 500 for it.
#[derive(Debug)]
pub(super) struct Overtaken {
    method: String,
    /// The addresses its 500 reached: its sender, and each address its
    /// sender passed a 500 to a request of its method on to after that
    /// refusal (a B2BUA relaying the refusal to the party whose request it
    /// carried).
    refused: Vec<SocketAddr>,
    /// For a PRACK, the addresses the give-up of the INVITE whose reliable
    /// provisional it acknowledged reached (RFC 3262 §3): its sender, which
    /// its receiver sent a 5xx final to an INVITE of its Call-ID after the
    /// refusal, and each address that sender passed a 5xx final to an INVITE
    /// on to after it; empty for any other method, or with no such final.
    invite_failed: Vec<SocketAddr>,
    /// Whether the recording shows a 2xx final to an INVITE of its Call-ID:
    /// its leg's call got past setup.
    answered: bool,
    /// Whether it is a REFER or an INVITE inside its dialog (a To-tag) and no
    /// later request of its sender of that method on that dialog (its
    /// Call-ID, From-tag and To-tag, a CSeq above its own) took a 2xx after
    /// the refusal: the transfer or the session change it asked for never
    /// happened.
    left_undone: bool,
}

/// Every request `events` show refused 500 after the loss of its first copy
/// and after its sender sent a request of the same dialog numbered above it.
pub(super) fn overtaken_requests(events: &[Stamped<SignalingNetworkEvent>]) -> Vec<Overtaken> {
    (0..events.len()).filter_map(|at| overtaken_at(events, at)).collect()
}

/// Whether a failure of the call at `at` for `cause` is the overtaken
/// request's own transaction or its direct consequence on its leg, as the
/// wire shows them, `party` naming the address of each role, none for a
/// barrier. Excused, for some `overtaken` request R, are only:
///
/// - R's refusal: a `WrongStatus` 500 met by a party R's 500 reached (the
///   status names no method, so a 500 that party met to another request of
///   it counts too);
/// - R a PRACK, the give-up of the INVITE whose reliable provisional it
///   acknowledged (RFC 3262 §3): a `WrongStatus` 5xx at the `invite` goal of
///   a party that give-up reached;
/// - the call state the scenario waited for behind R: a barrier's `Timeout`,
///   whichever wait gave up on it, on a call whose INVITE on R's leg took no
///   2xx, or with R a REFER or an in-dialog INVITE left undone (no later one
///   of its method on its dialog took a 2xx).
///
/// A barrier's timeout on a call R's leg set up behind any other R, or behind
/// a REFER or an in-dialog INVITE a later one of its method made good, any
/// other status or party, and any other cause stay failures.
pub(super) fn excuses(
    overtaken: &[Overtaken],
    at: &FailedAt,
    cause: &StepError,
    party: impl Fn(&str) -> Option<SocketAddr>,
) -> bool {
    let among = |addrs: &[SocketAddr], who: &str| party(who).is_some_and(|a| addrs.contains(&a));
    let at_invite = at.goal.is_some_and(|(kind, _)| kind == "invite");
    overtaken.iter().any(|r| match cause {
        StepError::WrongStatus { who, got, .. } => {
            (*got == 500 && among(&r.refused, who))
                || ((500..600).contains(got) && at_invite && among(&r.invite_failed, who))
        }
        StepError::Timeout { who, .. } => party(who).is_none() && (!r.answered || r.left_undone),
        _ => false,
    })
}

/// The request `events[at]` names when it is the `SendLost` of a request's
/// first copy that the events after it show overtaken and refused 500.
fn overtaken_at(events: &[Stamped<SignalingNetworkEvent>], at: usize) -> Option<Overtaken> {
    let SignalingNetworkEvent::SendLost { bind_key: lane, msg, .. } = &events[at].event else {
        return None;
    };
    let after = &events[at + 1..];
    let lost = sniff::identity(msg);
    let method = lost.method.filter(|m| !matches!(*m, "ACK" | "CANCEL"))?;
    lost.cseq?;
    let from_tag = sniff::from_tag(msg);
    let overtaking = after.iter().position(|e| {
        sent(e).is_some_and(|(l, _, next)| l == lane && numbered_above(&lost, &from_tag, next))
    })?;
    let after = &after[overtaking + 1..];
    let refusal = after.iter().position(|e| {
        sent(e).is_some_and(|(l, _, answer)| l != lane && refuses_500(&lost, answer))
    })?;
    let (refusal_lane, sender, _) = sent(&after[refusal])?;
    let relayed = after[refusal + 1..].iter().filter_map(sent).filter_map(|(l, dst, answer)| {
        let id = sniff::identity(answer);
        (l == lane && id.status == Some(500) && id.cseq_method == Some(method)).then_some(dst)
    });
    let refused = std::iter::once(sender).chain(relayed).collect();
    let invite_failed = match method {
        "PRACK" => invite_given_up(&after[refusal + 1..], refusal_lane, sender, lane, &lost),
        _ => Vec::new(),
    };
    let answered = events.iter().filter_map(sent).any(|(_, _, raw)| {
        let id = sniff::identity(raw);
        id.status.is_some_and(|s| (200..300).contains(&s))
            && id.cseq_method == Some("INVITE")
            && id.call_id == lost.call_id
    });
    let mid_call = lost.to_tag.is_some() && matches!(method, "REFER" | "INVITE");
    let left_undone = mid_call && !made_good(&after[refusal + 1..], method, &lost, &from_tag);
    Some(Overtaken { method: method.to_string(), refused, invite_failed, answered, left_undone })
}

/// Whether the events `after` the refusal show a 2xx to a later request of
/// `lost`'s sender of `method` on its dialog: its Call-ID, From-tag and
/// To-tag, a CSeq above `lost`'s.
fn made_good(
    after: &[Stamped<SignalingNetworkEvent>],
    method: &str,
    lost: &Identity<'_>,
    from_tag: &str,
) -> bool {
    after.iter().filter_map(sent).any(|(_, _, raw)| {
        let id = sniff::identity(raw);
        id.status.is_some_and(|s| (200..300).contains(&s))
            && id.cseq_method == Some(method)
            && id.call_id == lost.call_id
            && id.to_tag == lost.to_tag
            && sniff::from_tag(raw) == from_tag
            && id.cseq > lost.cseq
    })
}

/// The addresses the give-up of the INVITE reached, as the events `after`
/// the refusal show it: a 5xx final to an INVITE of `lost`'s Call-ID that
/// its receiver (on `receiver`) sent its sender at `sender`, then each
/// address that sender (on `lane`) passed a 5xx final to an INVITE on to.
fn invite_given_up(
    after: &[Stamped<SignalingNetworkEvent>],
    receiver: &LaneKey,
    sender: SocketAddr,
    lane: &LaneKey,
    lost: &Identity<'_>,
) -> Vec<SocketAddr> {
    let invite_5xx = |raw: &[u8]| {
        let id = sniff::identity(raw);
        id.status.is_some_and(|s| (500..600).contains(&s)) && id.cseq_method == Some("INVITE")
    };
    let Some(given_up) = after.iter().position(|e| {
        sent(e).is_some_and(|(l, dst, raw)| {
            l == receiver
                && dst == sender
                && invite_5xx(raw)
                && sniff::identity(raw).call_id == lost.call_id
        })
    }) else {
        return Vec::new();
    };
    let passed_on = after[given_up + 1..]
        .iter()
        .filter_map(sent)
        .filter(|(l, _, raw)| *l == lane && invite_5xx(raw))
        .map(|(_, dst, _)| dst);
    std::iter::once(sender).chain(passed_on).collect()
}

/// The lane, the destination and the bytes of a `SendCalled`.
fn sent(e: &Stamped<SignalingNetworkEvent>) -> Option<(&LaneKey, SocketAddr, &[u8])> {
    match &e.event {
        SignalingNetworkEvent::SendCalled { bind_key, to, msg } => Some((bind_key, *to, msg)),
        _ => None,
    }
}

/// Whether `next` is a request of `lost`'s dialog (its Call-ID, From-tag and
/// To-tag) numbered above it.
fn numbered_above(lost: &Identity<'_>, from_tag: &str, next: &[u8]) -> bool {
    let id = sniff::identity(next);
    id.method.is_some_and(|m| !matches!(m, "ACK" | "CANCEL"))
        && id.call_id == lost.call_id
        && id.to_tag == lost.to_tag
        && sniff::from_tag(next) == from_tag
        && id.cseq > lost.cseq
}

/// Whether `answer` is a 500 to `lost`'s transaction: its Call-ID, CSeq,
/// method and top-Via branch.
fn refuses_500(lost: &Identity<'_>, answer: &[u8]) -> bool {
    let id = sniff::identity(answer);
    id.status == Some(500)
        && id.call_id == lost.call_id
        && id.cseq == lost.cseq
        && id.cseq_method == lost.method
        && lost.branch.is_some()
        && id.branch == lost.branch
}

#[cfg(test)]
mod tests;
