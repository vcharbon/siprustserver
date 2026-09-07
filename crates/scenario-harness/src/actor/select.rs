//! WHICH parked request an [`ExpectRequest`](super::goals::GoalStep::ExpectRequest)
//! consumes when several of its kind are parked at once.
//!
//! The pick is a correlation, not a queue. Three criteria, in precedence order:
//!
//! 1. the request's KIND (a step never consumes another method);
//! 2. the step's expected CSeq RANK on the leg, when the caller supplied one;
//! 3. the step's [`BodyExpect`], as a discriminator between two same-method
//!    requests of which only one carries a session description;
//!
//! and arrival order decides what none of them does. A leg carrying one request
//! of a kind at a time is unaffected: the single candidate answers every
//! criterion.
//!
//! Positional (arrival-order) pairing alone is wrong exactly when the peer sends
//! a second request of a kind before the first is settled — the overtaking
//! re-INVITE — because from the first mis-pick onward every later request of
//! that kind pairs one off.

use sip_message::SipRequest;

use super::goals::{BodyExpect, RequestKind};
use super::runner::ParkedRequest;
use super::script::parked_matches;

/// The index of the parked request this expectation consumes, `None` when none
/// of them is of the kind.
///
/// `consumed` is how many requests of this kind the script has already consumed
/// on this leg — the anchor that turns a position among the CURRENTLY parked
/// requests into a rank over the leg's whole stream of that kind.
pub(super) fn select_parked(
    parked: &[ParkedRequest],
    kind: &RequestKind,
    body: BodyExpect,
    rank: Option<usize>,
    consumed: usize,
) -> Option<usize> {
    let candidates: Vec<usize> = parked
        .iter()
        .enumerate()
        .filter(|(_, p)| parked_matches(p, kind))
        .map(|(i, _)| i)
        .collect();
    let first = *candidates.first()?;
    if let Some(i) = by_rank(parked, &candidates, rank, consumed) {
        return Some(i);
    }
    // The body discriminates only while it leaves a candidate: a step whose only
    // candidate carries the wrong body still consumes it and fails on the body
    // assertion, never by starving the goal into a timeout.
    Some(
        candidates
            .iter()
            .copied()
            .find(|&i| {
                let (len, is_sdp) = body_facts(parked[i].txn.request());
                body.satisfied_by(len, is_sdp)
            })
            .unwrap_or(first),
    )
}

/// The candidate whose rank on the leg is the one the step expects.
///
/// A candidate's rank is `consumed` plus its position among the parked requests
/// of its kind ordered by ASCENDING CSeq — the leg's own ordering of the
/// transactions, which arrival order reproduces only while nothing overtakes.
/// A rank no candidate carries yields `None`: the caller's number is a
/// correlation hint, never a filter that can starve a reception.
fn by_rank(
    parked: &[ParkedRequest],
    candidates: &[usize],
    rank: Option<usize>,
    consumed: usize,
) -> Option<usize> {
    let want = rank?;
    let mut ordered = candidates.to_vec();
    ordered.sort_by_key(|&i| parked[i].txn.request().cseq().seq());
    ordered.get(want.checked_sub(consumed)?).copied()
}

/// The two facts a [`BodyExpect`] answers to: the body's length, and whether its
/// media type is a session description.
pub(super) fn body_facts(req: &SipRequest) -> (usize, bool) {
    let is_sdp = !req.body().is_empty()
        && req
            .header::<sip_message::header::MediaType>()
            .and_then(Result::ok)
            .is_some_and(|media| media.token().to_ascii_lowercase().contains("sdp"));
    (req.body().len(), is_sdp)
}
