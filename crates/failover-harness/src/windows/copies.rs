//! The predicates of the windows two acting copies, or a copy and a dead
//! owner, open (ADR-0055 §11): a CANCEL of an INVITE another owner saw
//! answered, and a non-2xx after another owner's final to the same INVITE.

use std::net::SocketAddr;

use sip_message::Method;
use sip_net_testkit::{CaptureIndex, Captured};

/// The audit rule a caller's unACKed non-2xx INVITE final meets.
pub(super) const RULE_UNACKED_NON_2XX: &str = "unacked-invite-non-2xx-final";

/// The audit rule a UAS final under a To-tag none of its provisionals carried
/// meets.
pub(super) const RULE_TAG_CONSISTENCY: &str = "tag-consistency";

/// The audit rule a BYE outside a confirmed dialog meets.
pub(super) const RULE_EARLY_BYE: &str = "no-bye-outside-or-early-dialog";

/// The audit rule a UAS answering 2xx an INVITE it took a CANCEL for meets.
pub(super) const RULE_NO_200_AFTER_CANCEL: &str = "no-200-after-cancel";

/// The audit rule a response to an INVITE meets that follows, at its taker,
/// another sender's final to it.
pub(super) const RULE_TAKEN_FINAL: &str = "taken-final-ends-invite-txn";

/// Whether the CANCEL at index `i` of `capture` names an INVITE an entry
/// before it, captured at or before `upto`, answered with a final.
pub(super) fn cancels_an_answered_invite(i: usize, capture: &CaptureIndex, upto: u64) -> bool {
    let Some(cancel) = capture.message(i).filter(|m| m.method().is_some()) else { return false };
    let ours = |resp: &Captured| {
        resp.status().is_some_and(|s| s >= 200) && resp.from_tag == cancel.from_tag
    };
    answered_before(capture, i, upto, cancel, None, ours)
}

/// Whether the final at index `i` of `capture` is a non-2xx to an INVITE
/// after a 2xx to that INVITE sent to the same taker at or before
/// `opened_at`, the kill.
pub(super) fn non_2xx_after_the_dead_owners_2xx(
    i: usize,
    capture: &CaptureIndex,
    opened_at: u64,
) -> bool {
    let taker = capture.entry(i).map(|e| e.to);
    taker.is_some() && non_2xx_after_a_2xx(i, capture, taker, opened_at)
}

/// Whether the response at index `i` of `capture` is a 2xx to an INVITE a
/// 487 to it, sent to the same taker, came before: the callee's answer to a
/// call asked again after the caller's CANCEL.
pub(super) fn two_xx_after_a_487(i: usize, capture: &CaptureIndex) -> bool {
    let (Some(entry), Some(resp)) = (capture.entry(i), capture.message(i)) else { return false };
    let terminated = |r: &Captured| r.status() == Some(487);
    resp.status().is_some_and(|s| (200..300).contains(&s))
        && resp.cseq_method == Method::Invite
        && answered_before(capture, i, u64::MAX, resp, Some(entry.to), terminated)
}

/// Whether the final at index `i` of `capture` is a non-2xx to an INVITE,
/// sent to `to` when it is stated, after a 2xx to that INVITE sent there and
/// captured at or before `upto`.
fn non_2xx_after_a_2xx(
    i: usize,
    capture: &CaptureIndex,
    to: Option<SocketAddr>,
    upto: u64,
) -> bool {
    let (Some(entry), Some(resp)) = (capture.entry(i), capture.message(i)) else { return false };
    if to.is_some_and(|to| entry.to != to) {
        return false;
    }
    let answer = |r: &Captured| r.status().is_some_and(|s| (200..300).contains(&s));
    is_invite_non_2xx(resp) && answered_before(capture, i, upto, resp, to, answer)
}

/// Whether the final at index `i` of `capture` is a non-2xx to an INVITE
/// after another final to it: a 2xx, or a non-2xx of another status.
pub(super) fn non_2xx_after_another_final(i: usize, capture: &CaptureIndex) -> bool {
    let Some(resp) = capture.message(i) else { return false };
    let other = |r: &Captured| r.status().is_some_and(|s| s >= 200) && r.status() != resp.status();
    is_invite_non_2xx(resp) && answered_before(capture, i, u64::MAX, resp, None, other)
}

/// Whether the BYE at index `i` of `capture` re-uses the CSeq number of
/// another BYE of its Call-ID captured before it, under another branch: two
/// copies each ended the dialog at one CSeq.
pub(super) fn second_bye_at_one_cseq(i: usize, capture: &CaptureIndex) -> bool {
    let not_judged = [Method::Ack, Method::Cancel];
    let Some(key) = capture.message(i).and_then(|m| m.request_txn(&not_judged)) else {
        return false;
    };
    key.method == Method::Bye
        && capture.before(i).any(|(_, _, m)| {
            m.request_txn(&not_judged).is_some_and(|other| {
                other.method == Method::Bye
                    && other.call_id == key.call_id
                    && other.cseq == key.cseq
                    && other.branch != key.branch
            })
        })
}

/// Whether `resp` is a non-2xx final to an INVITE.
fn is_invite_non_2xx(resp: &Captured) -> bool {
    resp.status().is_some_and(|s| s >= 300) && resp.cseq_method == Method::Invite
}

/// Whether an entry before index `i` of `capture`, captured at or before
/// `upto`, is a response to the INVITE `of` names (its Call-ID and CSeq
/// number), sent to `to` when it is stated, of which `also` holds.
fn answered_before(
    capture: &CaptureIndex,
    i: usize,
    upto: u64,
    of: &Captured,
    to: Option<SocketAddr>,
    also: impl Fn(&Captured) -> bool,
) -> bool {
    capture.before(i).filter(|(_, e, _)| e.seq <= upto && to.is_none_or(|to| e.to == to)).any(
        |(_, _, resp)| {
            resp.status().is_some()
                && resp.cseq_method == Method::Invite
                && resp.cseq == of.cseq
                && resp.call_id == of.call_id
                && also(resp)
        },
    )
}
