//! RFC 3261 §14 — an INVITE transaction INSIDE a confirmed dialog: how many may
//! be in flight, what answers one that races another, and how one that fails
//! must leave the dialog. THREE obligations, each read off one endpoint's own
//! view:
//!
//!   - [`NoReInviteWhileInviteInProgress`] (§14.1) charges the UAC: it holds
//!     one INVITE outstanding per dialog DIRECTION and waits for Confirmed.
//!   - [`ConcurrentReInvite500Or491`] (§14.2) charges the UAS: a re-INVITE that
//!     arrives while another INVITE of the dialog is in progress is answered
//!     491, or 500 with `Retry-After`.
//!   - [`FailedReinviteTearsDownDialog`] (§14.1 / §17.1.1.2) charges the UAC's
//!     peer path: a re-INVITE that drew a provisional draws a final too — the
//!     prior session survives a failed re-INVITE, and silence ends it.
//!
//! **The two sides key on DIFFERENT dialog identities, and that is the point.**
//! §14.1 forbids a UAC from overlapping its OWN requests, so its key carries the
//! ORDERED tag pair: crossing re-INVITEs — one per direction — are legal glare
//! (§14.2 has the peers 491 each other), and a relay face that forwards both
//! directions of one dialog must not read them as one UAC overlapping. §14.2
//! judges a UAS against everything it has in flight on the dialog whichever way
//! it points, so its key carries the UNORDERED pair.
//!
//! **A same-branch repeat is never a second act.** An INVITE repeating a branch
//! already in flight is a Timer-A retransmission, and one repeating a branch
//! already answered is what §17.2.1 obliges the server transaction to replay a
//! final for: neither opens an occasion, and neither re-enters the in-progress
//! window.
//!
//! **A 2xx does not end an INVITE transaction — the ACK does** (§14.1 rule 2,
//! RFC 6026's *Accepted* interval). A non-2xx final does end it there and then:
//! its ACK belongs to the transaction layer and proves nothing about the TU.
//!
//! **No top-Via branch, no occasion.** The branch is how an INVITE is paired
//! with its own answers (§17); without one nothing can say what is still in
//! flight, so the rules state nothing rather than guessing.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::branch::BranchReading;
use super::Obligation;

/// How long an OPEN observation must keep running past a racing re-INVITE
/// before "the taker answered it nothing" reads as silence rather than
/// truncation. One second — the §17.2 reasoning of
/// [`super::capability::ANSWER_WINDOW_US`]; in a CLOSED observation it
/// collapses.
pub const REINVITE_ANSWER_WINDOW_US: u64 = 1_000_000;

/// How long an OPEN observation must keep running past a re-INVITE's last
/// provisional before "no final ever came" reads as abandonment rather than
/// truncation.
///
/// Three minutes: §16.6 gives Timer C at least that, so a proxy on the path may
/// legitimately hold an INVITE ringing that long before it gives up and
/// produces one. In a CLOSED observation it collapses.
pub const REINVITE_FINAL_WINDOW_US: u64 = 180_000_000;

/// **RFC 3261 §14.2 — a racing re-INVITE is answered 491, or 500 with
/// Retry-After.** A UAS serialises offer/answer: while one INVITE transaction
/// of a dialog is still in progress, a second one cannot be evaluated against a
/// session state that is itself mid-change. §14.2 has it say so rather than
/// answer both — the test UA answers both, so nothing else catches it.
///
/// The occasion is ONE re-INVITE the endpoint TOOK while another INVITE of that
/// dialog was in progress at it. Charges the taker; 491, or 500 carrying
/// `Retry-After`, discharges it.
///
/// **In progress is measured on the UNORDERED dialog**: a UAS answers whatever
/// INVITEs ride the dialog, whichever party sent them, so a callee-initiated
/// re-INVITE crossing a caller-initiated one is exactly the race §14.2 is
/// about.
///
/// **Only the FINAL answers §14.2.** A 100 Trying precedes a compliant 491 and
/// must not stand in for it, so the verdict reads the transaction's first final
/// and nothing earlier.
pub struct ConcurrentReInvite500Or491;

impl Obligation for ConcurrentReInvite500Or491 {
    fn id(&self) -> RuleId {
        RuleId::ConcurrentReInvite500Or491
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per endpoint's view of a dialog: the branches of the INVITE
        // transactions still in progress at it. Built as the walk goes, so a
        // re-INVITE is judged against what was in flight BEFORE it.
        let mut in_progress: BTreeMap<PairKey<'_>, BTreeSet<&str>> = BTreeMap::new();
        let mut dialog_of: BTreeMap<TxnKey<'_>, PairKey<'_>> = BTreeMap::new();
        let mut completed: BTreeSet<TxnKey<'_>> = BTreeSet::new();
        // The racing re-INVITEs, and the first final each was answered with.
        let mut raced: Vec<Race<'_>> = Vec::new();
        let mut answered: BTreeMap<TxnKey<'_>, Answer> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    let taker = msg.dst.as_str();
                    let (Some(from_tag), Some(to_tag)) = tags(msg) else { continue };
                    let txn = TxnKey { endpoint: taker, call_id: msg.call_id.as_str(), branch };
                    // §17.2.1: an INVITE repeating an ALREADY-ANSWERED branch
                    // draws the recorded final again — never a new transaction,
                    // and never back into the in-progress window.
                    if completed.contains(&txn) {
                        continue;
                    }
                    let dialog = PairKey::of(taker, &msg.call_id, from_tag, to_tag);
                    let in_flight = in_progress.entry(dialog).or_default();
                    // A Timer-A retransmission reuses its own branch.
                    if !in_flight.contains(branch) {
                        if let Some(pending) = in_flight.iter().next().copied() {
                            raced.push(Race {
                                txn,
                                msg: mi,
                                hop: msg.hop,
                                ts_us: msg.at_us,
                                cseq: msg.cseq,
                                sender: msg.src.as_str(),
                                branch,
                                pending_branch: pending,
                            });
                        }
                    }
                    in_flight.insert(branch);
                    dialog_of.insert(txn, dialog);
                }
                Kind::Response { status } if msg.cseq_method.eq_ignore_ascii_case("INVITE") => {
                    let txn =
                        TxnKey { endpoint: msg.src.as_str(), call_id: msg.call_id.as_str(), branch };
                    if *status < 200 || *status >= 700 {
                        continue;
                    }
                    answered.entry(txn).or_insert(Answer {
                        status: *status,
                        // §14.2 makes `Retry-After` part of the 500 answer; a
                        // vantage carrying no header block cannot read it.
                        retry_after: msg.head.as_deref().map(|h| sniff::has_header(h, "retry-after")),
                    });
                    if let Some(dialog) = dialog_of.get(&txn) {
                        if let Some(in_flight) = in_progress.get_mut(dialog) {
                            in_flight.remove(branch);
                        }
                    }
                    completed.insert(txn);
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for race in raced {
            let finding = |decision| Finding {
                rule: RuleId::ConcurrentReInvite500Or491,
                emitter: race.txn.endpoint.to_string(),
                taker: race.sender.to_string(),
                cseq: race.cseq,
                relayed: false,
                anchor: race.msg,
                decision,
            };
            let pending_msg = raced_pending_msg(wire.msgs, &race);
            let Some(answer) = answered.get(&race.txn) else {
                // No final at all: an absence, decidable only once the
                // observation demonstrably outlived the window.
                if !wire.obs.absence_decidable(race.ts_us, REINVITE_ANSWER_WINDOW_US) {
                    out.push(finding(Decision::Undecidable(
                        "the observation stopped inside the answer window — truncation, not \
                         silence",
                    )));
                    continue;
                }
                out.push(finding(Decision::Violated(Evidence::ConcurrentReInvite {
                    concurrent_invite_msg: race.msg,
                    concurrent_invite_hop: race.hop,
                    concurrent_invite_ts_us: race.ts_us,
                    branch: race.branch.to_string(),
                    pending_invite_branch: race.pending_branch.to_string(),
                    pending_invite_msg: pending_msg,
                    answered_status: 0,
                    retry_after: false,
                })));
                continue;
            };
            if answer.status == 491 {
                out.push(finding(Decision::Compliant));
                continue;
            }
            let retry_after = match answer.retry_after {
                Some(present) => present,
                // Only a 500 turns on the header; anything else is already
                // decided by its status alone.
                None if answer.status == 500 => {
                    out.push(finding(Decision::Undecidable(
                        "no header block at this vantage",
                    )));
                    continue;
                }
                None => false,
            };
            if answer.status == 500 && retry_after {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::ConcurrentReInvite {
                concurrent_invite_msg: race.msg,
                concurrent_invite_hop: race.hop,
                concurrent_invite_ts_us: race.ts_us,
                branch: race.branch.to_string(),
                pending_invite_branch: race.pending_branch.to_string(),
                pending_invite_msg: pending_msg,
                answered_status: answer.status,
                retry_after,
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §14.1 — one INVITE outstanding per dialog direction.** A UAC that
/// issues a second in-dialog INVITE before its first has finished leaves the
/// peer holding two offers for one session; §14.1 has it wait, and a real peer
/// 491s the overtaking one. The test UA answers both.
///
/// The occasion is ONE in-dialog INVITE an endpoint SENT. Charges that endpoint;
/// having nothing of its own outstanding on that dialog direction discharges it.
///
/// **Outstanding means not yet Confirmed.** A 2xx puts the transaction in RFC
/// 6026's *Accepted* state, where it stays until the UAC's own ACK goes out —
/// §14.1 rule 2 requires Confirmed, not merely a final. A non-2xx final ends it
/// at the final itself, because its ACK is the transaction layer's and may
/// never surface as a TU send at all.
///
/// **The key is the ORDERED tag pair — same-direction overlap only.** The
/// reversed orientation is the OTHER party's request: crossing re-INVITEs, one
/// per direction, are legal glare (§14.1/§14.2 have the peers answer per leg),
/// and one forwarding face relaying both directions of a dialog would otherwise
/// be charged for a race neither party ran.
///
/// **The initial INVITE opens no occasion** — it creates the dialog rather than
/// riding one — but it IS tracked: it stays outstanding until its own ACK, and
/// re-keys onto the dialog id the moment its 2xx names one, so a re-INVITE
/// overtaking that ACK still collides.
pub struct NoReInviteWhileInviteInProgress;

impl Obligation for NoReInviteWhileInviteInProgress {
    fn id(&self) -> RuleId {
        RuleId::NoReInviteWhileInviteInProgress
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per requester (endpoint + dialog + DIRECTION): the branches of its own
        // INVITE transactions still outstanding.
        let mut in_progress: BTreeMap<OrderedKey<'_>, BTreeSet<&str>> = BTreeMap::new();
        let mut dialog_of: BTreeMap<TxnKey<'_>, OrderedKey<'_>> = BTreeMap::new();
        // The CSeq an INVITE branch spent: the ACK of a 2xx is its own
        // transaction (a fresh branch, §17.1.1.3), so it is paired back to the
        // INVITE it confirms by dialog + CSeq rather than by branch.
        let mut branch_cseq: BTreeMap<TxnKey<'_>, u32> = BTreeMap::new();
        // Transactions whose 2xx has landed and whose ACK has not gone out.
        let mut accepted: BTreeSet<TxnKey<'_>> = BTreeSet::new();
        let mut out = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("ACK") => {
                    let sender = msg.src.as_str();
                    let from_tag = msg.from_tag.as_deref().unwrap_or_default();
                    let to_tag = msg.to_tag.as_deref().unwrap_or_default();
                    // The pre-answer (From-only) key too: a branch parks there
                    // until its 2xx re-keys it, and an unmatched trace may hold
                    // it either way.
                    for dialog in [
                        OrderedKey { endpoint: sender, call_id: msg.call_id.as_str(), from_tag, to_tag },
                        OrderedKey {
                            endpoint: sender,
                            call_id: msg.call_id.as_str(),
                            from_tag,
                            to_tag: "",
                        },
                    ] {
                        let Some(in_flight) = in_progress.get_mut(&dialog) else { continue };
                        in_flight.retain(|b| {
                            let txn =
                                TxnKey { endpoint: sender, call_id: msg.call_id.as_str(), branch: b };
                            let confirms = branch_cseq.get(&txn) == Some(&msg.cseq);
                            if confirms {
                                accepted.remove(&txn);
                            }
                            !confirms
                        });
                    }
                }
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty())
                    else {
                        continue;
                    };
                    let sender = msg.src.as_str();
                    let txn = TxnKey { endpoint: sender, call_id: msg.call_id.as_str(), branch };
                    let from_tag = msg.from_tag.as_deref().unwrap_or_default();
                    let to_tag = msg.to_tag.as_deref().unwrap_or_default();
                    let in_dialog = !from_tag.is_empty() && !to_tag.is_empty();
                    let dialog = OrderedKey {
                        endpoint: sender,
                        call_id: msg.call_id.as_str(),
                        from_tag,
                        // The dialog-creating INVITE has no peer tag yet: it
                        // parks on the From-only key until its 2xx names one.
                        to_tag: if in_dialog { to_tag } else { "" },
                    };
                    let in_flight = in_progress.entry(dialog).or_default();
                    if in_dialog {
                        let finding = |decision| Finding {
                            rule: RuleId::NoReInviteWhileInviteInProgress,
                            emitter: sender.to_string(),
                            taker: msg.dst.to_string(),
                            cseq: msg.cseq,
                            relayed: false,
                            anchor: mi,
                            decision,
                        };
                        // A Timer-A retransmission reuses its own branch: the
                        // same act again, never a second one.
                        let prior = if in_flight.contains(branch) {
                            None
                        } else {
                            in_flight.iter().next().copied()
                        };
                        match prior {
                            None => out.push(finding(Decision::Compliant)),
                            Some(prior_branch) => {
                                let prior_txn = TxnKey {
                                    endpoint: sender,
                                    call_id: msg.call_id.as_str(),
                                    branch: prior_branch,
                                };
                                out.push(finding(Decision::Violated(
                                    Evidence::OverlappingReInvite {
                                        overtaking_invite_msg: mi,
                                        overtaking_invite_hop: msg.hop,
                                        overtaking_invite_ts_us: msg.at_us,
                                        branch: branch.to_string(),
                                        prior_branch: prior_branch.to_string(),
                                        prior_msg: branch_msg(wire.msgs, sender, prior_branch),
                                        prior_accepted: accepted.contains(&prior_txn),
                                    },
                                )));
                            }
                        }
                    }
                    branch_cseq.insert(txn, msg.cseq);
                    in_flight.insert(branch);
                    dialog_of.insert(txn, dialog);
                }
                Kind::Response { status }
                    if *status >= 200 && msg.cseq_method.eq_ignore_ascii_case("INVITE") =>
                {
                    let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty())
                    else {
                        continue;
                    };
                    // The transaction belongs to the endpoint that TOOK the
                    // response — the UAC side of the branch.
                    let uac = msg.dst.as_str();
                    let txn = TxnKey { endpoint: uac, call_id: msg.call_id.as_str(), branch };
                    let Some(dialog) = dialog_of.get(&txn).copied() else { continue };
                    let still_open = in_progress
                        .get_mut(&dialog)
                        .is_some_and(|in_flight| in_flight.remove(branch));
                    if *status >= 300 {
                        // Non-2xx: the transaction layer ACKs it inside the same
                        // transaction — over at the final.
                        accepted.remove(&txn);
                        continue;
                    }
                    if !still_open {
                        continue;
                    }
                    // 2xx: *Accepted*, not over. The dialog-creating INVITE
                    // re-keys off the From-only key onto the now-named dialog,
                    // so a re-INVITE overtaking its ACK collides with it.
                    let confirmed = OrderedKey {
                        endpoint: uac,
                        call_id: msg.call_id.as_str(),
                        from_tag: msg.from_tag.as_deref().unwrap_or_default(),
                        to_tag: msg.to_tag.as_deref().unwrap_or_default(),
                    };
                    in_progress.entry(confirmed).or_default().insert(branch);
                    dialog_of.insert(txn, confirmed);
                    accepted.insert(txn);
                }
                _ => {}
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §14.1 — a failed re-INVITE leaves the dialog in its prior
/// state.** A non-2xx final to an in-dialog INVITE does not end the call: the
/// prior session continues and the failure is merely reported. A path that
/// drops the re-INVITE's transaction state on a relayed PROVISIONAL lets the
/// non-2xx final that follows fall through a failure route and destroy the
/// bridged call silently — the originator's transaction then sees a provisional
/// and NO final, which §17.1.1.2 also forbids, and the prior session is gone.
///
/// The occasion is ONE in-dialog INVITE an endpoint SENT that drew at least one
/// provisional: without a provisional nothing says the transaction was ever
/// live, and an INVITE that drew silence throughout is the transaction layer's
/// timeout, not a §14.1 prior-state breach. Charges the sender; any final on
/// the branch discharges it.
///
/// **"Absent another cause."** An independent teardown — a max-duration timer,
/// a keepalive give-up, an ACK watchdog, a peer BYE — abandons any in-flight
/// re-INVITE as a SIDE EFFECT, and is itself signalled by a BYE on the dialog,
/// which is a legitimate §15 teardown for a different reason. So a BYE anywhere
/// on that call at that endpoint discharges the occasion; the violation is the
/// re-INVITE that died on a dialog nothing ever ended.
///
/// **The silent-destroy shape has no BYE to key on**, which is exactly why this
/// rule keys on the abandoned transaction instead: the buggy path emits nothing
/// at all, so a BYE-keyed rule cannot see it.
pub struct FailedReinviteTearsDownDialog;

impl Obligation for FailedReinviteTearsDownDialog {
    fn id(&self) -> RuleId {
        RuleId::FailedReinviteTearsDownDialog
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        // The calls each endpoint saw a BYE on, in either direction — the
        // "another cause" escape.
        let mut byed: BTreeSet<(&str, &str)> = BTreeSet::new();
        for msg in wire.msgs {
            if msg.is_request("BYE") {
                byed.insert((msg.src.as_str(), msg.call_id.as_str()));
                byed.insert((msg.dst.as_str(), msg.call_id.as_str()));
            }
        }

        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            // A transaction that drew no provisional was never demonstrably
            // live: §17.1.1.2's timeout owns it, not §14.1.
            let Some(provisional) = branch.first_1xx else { continue };
            for invite in branch.sent_of("INVITE") {
                // Only an in-dialog INVITE rides a prior state to preserve. An
                // abandoned INITIAL INVITE is a different obligation's.
                if wire.msgs[invite.msg].to_tag.as_deref().is_none_or(str::is_empty) {
                    continue;
                }
                let finding = |decision| Finding {
                    rule: RuleId::FailedReinviteTearsDownDialog,
                    emitter: key.emitter.to_string(),
                    taker: invite.taker.to_string(),
                    cseq: invite.cseq,
                    relayed: false,
                    anchor: invite.msg,
                    decision,
                };
                if branch.first_final.is_some() || byed.contains(&(key.emitter, key.call_id)) {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
                let at = wire.msgs[provisional].at_us;
                if !wire.obs.absence_decidable(at, REINVITE_FINAL_WINDOW_US) {
                    out.push(finding(Decision::Undecidable(
                        "the observation stopped inside the final-response window — truncation, \
                         not abandonment",
                    )));
                    continue;
                }
                out.push(finding(Decision::Violated(Evidence::AbandonedReInvite {
                    abandoned_invite_msg: invite.msg,
                    abandoned_invite_hop: invite.hop,
                    abandoned_invite_ts_us: invite.ts_us,
                    branch: key.branch.to_string(),
                    provisional_msg: provisional,
                    provisional_status: wire.msgs[provisional].status().unwrap_or(0),
                    window_us: wire.obs.last_us.saturating_sub(at),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One INVITE transaction as ONE endpoint drove it. Spelled out as a struct so
/// the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TxnKey<'a> {
    endpoint: &'a str,
    call_id: &'a str,
    branch: &'a str,
}

/// One endpoint's view of a dialog, tags UNORDERED: §14.2 judges a UAS against
/// every INVITE riding the dialog, whichever party sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PairKey<'a> {
    endpoint: &'a str,
    call_id: &'a str,
    /// The dialog's two tags, sorted, so both orientations map to one key.
    lo: &'a str,
    hi: &'a str,
}

impl<'a> PairKey<'a> {
    fn of(endpoint: &'a str, call_id: &'a str, a: &'a str, b: &'a str) -> Self {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        PairKey { endpoint, call_id, lo, hi }
    }
}

/// One endpoint's view of a dialog DIRECTION, tags ORDERED: §14.1 forbids a UAC
/// from overlapping its OWN requests, and the reversed orientation is its
/// peer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OrderedKey<'a> {
    endpoint: &'a str,
    call_id: &'a str,
    from_tag: &'a str,
    to_tag: &'a str,
}

/// One re-INVITE that arrived while another INVITE of its dialog was in flight.
#[derive(Debug)]
struct Race<'a> {
    txn: TxnKey<'a>,
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    /// The party that sent it — the other side of the §14.2 obligation.
    sender: &'a str,
    branch: &'a str,
    pending_branch: &'a str,
}

/// The first final one transaction was answered with.
#[derive(Debug)]
struct Answer {
    status: u16,
    /// Whether it carried `Retry-After`, or `None` where the vantage carried no
    /// header block to read.
    retry_after: Option<bool>,
}

/// A message's dialog tags, both present, or `None` where either is missing —
/// an INVITE without both names no dialog to serialise on.
fn tags(msg: &Msg) -> (Option<&str>, Option<&str>) {
    (
        msg.from_tag.as_deref().filter(|t| !t.is_empty()),
        msg.to_tag.as_deref().filter(|t| !t.is_empty()),
    )
}

/// View index of the INVITE `endpoint` sent on `branch`, or the branch's own
/// occasion index where this vantage carried no such send.
fn branch_msg(msgs: &[Msg], endpoint: &str, branch: &str) -> usize {
    msgs.iter()
        .position(|m| m.src == endpoint && m.is_request("INVITE") && names(m, branch))
        .unwrap_or(0)
}

/// View index of the INVITE the racing one collided with, as its TAKER saw it.
fn raced_pending_msg(msgs: &[Msg], race: &Race<'_>) -> usize {
    msgs.iter()
        .position(|m| {
            m.dst == race.txn.endpoint && m.is_request("INVITE") && names(m, race.pending_branch)
        })
        .unwrap_or(race.msg)
}

/// Whether `msg` carries `branch` as its top-Via branch.
fn names(msg: &Msg, branch: &str) -> bool {
    msg.via_branch.as_deref() == Some(branch)
}

#[cfg(test)]
mod tests {
    //! The family's OWN semantics under a CLOSED observation — what the live
    //! adapter's lane policy cannot state: which INVITEs are in flight
    //! together, which tag orientation a race is read on, and what the wire
    //! alone settles about an unanswered one.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        ConcurrentReInvite500Or491, FailedReinviteTearsDownDialog,
        NoReInviteWhileInviteInProgress, REINVITE_FINAL_WINDOW_US,
    };

    const ALICE: &str = "10.0.0.1:5060";
    const BOB: &str = "10.0.0.2:5060";

    /// A request on the wire, with caller-controlled tags and top-Via branch.
    #[allow(clippy::too_many_arguments)]
    fn req(
        at_us: u64,
        src: &str,
        dst: &str,
        method: &str,
        branch: &str,
        cseq: u32,
        from_tag: &str,
        to_tag: &str,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: method.to_string(),
            via_branch: Some(branch.to_string()),
            from_tag: Some(from_tag.to_string()),
            to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
            head: Some(b"Content-Length: 0\r\n\r\n".to_vec()),
            body: None,
        }
    }

    /// A response on the wire, answering an INVITE transaction on `branch`.
    #[allow(clippy::too_many_arguments)]
    fn rsp(
        at_us: u64,
        src: &str,
        dst: &str,
        status: u16,
        branch: &str,
        cseq: u32,
        from_tag: &str,
        to_tag: &str,
        head: &str,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Response { status },
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            via_branch: Some(branch.to_string()),
            from_tag: Some(from_tag.to_string()),
            to_tag: (!to_tag.is_empty()).then(|| to_tag.to_string()),
            head: Some(format!("{head}Content-Length: 0\r\n\r\n").into_bytes()),
            body: None,
        }
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    /// The same view with the observation left OPEN at `last_us`, so a window
    /// gate is exercised rather than collapsed.
    fn eval_open(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        let mut o = obs(msgs);
        o.closed = false;
        rule.eval(&WireView { msgs, obs: &o })
    }

    fn eval(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    fn hits(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        eval(rule, msgs).into_iter().filter(Finding::violated).collect()
    }

    // ---- concurrent-re-invite-500-or-491 ---------------------------------

    /// Two in-dialog INVITEs race at the UAS and the second draws 491: the
    /// §14.2 answer, so the occasion is met.
    #[test]
    fn a_racing_re_invite_answered_491_is_compliant() {
        let f = eval(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
                rsp(3_000, BOB, ALICE, 491, "z9hG4bK-2", 3, "at", "bt", ""),
                rsp(4_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
            ],
        );
        assert_eq!(f.len(), 1, "one occasion, the racing re-INVITE: {f:?}");
        assert_eq!(f[0].rule, RuleId::ConcurrentReInvite500Or491);
        assert_eq!(f[0].emitter, BOB, "the UAS that owed the 491 is charged");
        assert_eq!(f[0].taker, ALICE);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// Answering the race 200 serves two offers at once — the violation, with
    /// both transactions named.
    #[test]
    fn a_racing_re_invite_answered_200_is_violated() {
        let f = hits(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
                rsp(3_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
            ],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::ConcurrentReInvite {
            branch,
            pending_invite_branch,
            pending_invite_msg,
            answered_status,
            retry_after,
            ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!(branch.as_str(), "z9hG4bK-2");
        assert_eq!(pending_invite_branch.as_str(), "z9hG4bK-1");
        assert_eq!(*pending_invite_msg, 0, "the INVITE it collided with");
        assert_eq!((*answered_status, *retry_after), (200, false));
        assert_eq!(f[0].anchor, 1, "the occasion rests on the racing re-INVITE");
    }

    /// §14.2's other legal answer: 500 CARRYING `Retry-After`. A bare 500 is
    /// not it.
    #[test]
    fn a_500_discharges_only_with_retry_after() {
        let race = |head: &str| {
            vec![
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
                rsp(3_000, BOB, ALICE, 500, "z9hG4bK-2", 3, "at", "bt", head),
            ]
        };
        assert!(hits(&ConcurrentReInvite500Or491, &race("Retry-After: 5\r\n")).is_empty());
        assert_eq!(hits(&ConcurrentReInvite500Or491, &race("")).len(), 1);
    }

    /// The 100 Trying precedes a compliant 491 — only the transaction's FINAL
    /// answers §14.2, so the provisional must not stand in for it.
    #[test]
    fn a_100_trying_does_not_stand_in_for_the_final() {
        assert!(hits(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
                rsp(3_000, BOB, ALICE, 100, "z9hG4bK-2", 3, "at", "bt", ""),
                rsp(4_000, BOB, ALICE, 491, "z9hG4bK-2", 3, "at", "bt", ""),
                rsp(5_000, BOB, ALICE, 200, "z9hG4bK-1", 2, "at", "bt", ""),
            ],
        )
        .is_empty());
    }

    /// §17.2.1: a Timer-A retransmission of an ALREADY-ANSWERED re-INVITE
    /// crosses its own 491 on the wire. It draws the recorded final again — it
    /// is not a new transaction and must not re-enter the in-progress window,
    /// or the next genuine re-INVITE's compliant 200 reads as a race.
    #[test]
    fn a_post_final_retransmit_does_not_re_enter_the_window() {
        let f = hits(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                rsp(2_000, BOB, ALICE, 491, "z9hG4bK-1", 2, "at", "bt", ""),
                req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
                rsp(5_000, BOB, ALICE, 200, "z9hG4bK-2", 3, "at", "bt", ""),
            ],
        );
        assert!(f.is_empty(), "{f:?}");
    }

    /// A retransmission of an INVITE still in flight is the same act again,
    /// never a second transaction to serialise against.
    #[test]
    fn an_in_flight_retransmit_is_no_race() {
        assert!(eval(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            ],
        )
        .is_empty());
    }

    /// The race is read on the UNORDERED dialog: a callee-initiated re-INVITE
    /// crossing the caller's is exactly the §14.2 collision, tags reversed and
    /// all.
    #[test]
    fn crossing_re_invites_collide_at_the_uas_whichever_way_they_point() {
        let f = hits(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-a", 2, "at", "bt"),
                // BOB is the taker of the first and the SENDER of the reversed
                // one; only what it TOOK is judged here.
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-b", 2, "bt", "at"),
                rsp(3_000, BOB, ALICE, 200, "z9hG4bK-b", 2, "bt", "at", ""),
            ],
        );
        assert_eq!(f.len(), 1, "the reversed re-INVITE races the first: {f:?}");
    }

    /// An INVITE with only one tag creates a dialog rather than riding one:
    /// §14.2 has nothing to serialise it against.
    #[test]
    fn an_initial_invite_is_no_race() {
        assert!(eval(
            &ConcurrentReInvite500Or491,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", ""),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 2, "at", ""),
            ],
        )
        .is_empty());
    }

    /// A race the observation stopped on top of settles nothing: silence
    /// inside the window is truncation, not a missing answer.
    #[test]
    fn an_unanswered_race_inside_the_window_is_undecidable() {
        let msgs = [
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
        ];
        let open = eval_open(&ConcurrentReInvite500Or491, &msgs);
        assert_eq!(open.len(), 1, "{open:?}");
        assert!(!open[0].decided(), "{:?}", open[0].decision);
        // Closed, the window collapses and the absence decides.
        assert_eq!(hits(&ConcurrentReInvite500Or491, &msgs).len(), 1);
    }

    // ---- no-re-invite-while-invite-in-progress ---------------------------

    /// The clean shape: the prior INVITE reached Confirmed — 2xx AND the UAC's
    /// ACK — before the next one went out.
    #[test]
    fn a_re_invite_after_the_ack_is_compliant() {
        let f = eval(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", ""),
                rsp(2_000, BOB, ALICE, 200, "z9hG4bK-1", 1, "at", "bt", ""),
                req(3_000, ALICE, BOB, "ACK", "z9hG4bK-k", 1, "at", "bt"),
                req(4_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 2, "at", "bt"),
            ],
        );
        assert_eq!(f.len(), 1, "one occasion, the re-INVITE: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, ALICE, "the UAC that sent it is charged");
    }

    /// RFC 6026's *Accepted* interval: the 2xx has landed but the ACK has not
    /// gone out, so the prior transaction is still outstanding and the
    /// overtaking re-INVITE collides with it — on the INITIAL INVITE too, which
    /// re-keys onto the dialog its own 2xx names.
    #[test]
    fn a_re_invite_overtaking_the_ack_is_violated() {
        for (label, first) in [
            ("re-INVITE", req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt")),
            ("initial INVITE", req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 1, "at", "")),
        ] {
            let cseq = first.cseq;
            let f = hits(
                &NoReInviteWhileInviteInProgress,
                &[
                    first,
                    rsp(2_000, BOB, ALICE, 200, "z9hG4bK-1", cseq, "at", "bt", ""),
                    req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-2", cseq + 1, "at", "bt"),
                ],
            );
            assert_eq!(f.len(), 1, "{label}: {f:?}");
            let Decision::Violated(Evidence::OverlappingReInvite {
                prior_accepted, prior_branch, ..
            }) = &f[0].decision
            else {
                panic!("{label}: {:?}", f[0].decision)
            };
            assert!(prior_accepted, "{label}: the prior 2xx is unACKed — Accepted");
            assert_eq!(prior_branch.as_str(), "z9hG4bK-1");
        }
    }

    /// A non-2xx final ends the transaction at the final itself: its ACK is the
    /// transaction layer's and may never surface as a TU send, so the retry
    /// that follows is clean.
    #[test]
    fn a_re_invite_after_a_non_2xx_final_is_compliant() {
        assert!(hits(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                rsp(2_000, BOB, ALICE, 491, "z9hG4bK-1", 2, "at", "bt", ""),
                req(3_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            ],
        )
        .is_empty());
    }

    /// Nothing answered the first at all: the second overtakes a transaction
    /// still in progress.
    #[test]
    fn a_re_invite_over_an_unanswered_one_is_violated() {
        let f = hits(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-2", 3, "at", "bt"),
            ],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::OverlappingReInvite { prior_accepted, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert!(!prior_accepted, "no final arrived — in progress, not Accepted");
    }

    /// Crossing re-INVITEs relayed by ONE forwarding face are two requesters
    /// with one INVITE each — legal glare, not one UAC overlapping. The
    /// ORDERED key is what keeps them apart.
    #[test]
    fn relayed_crossing_glare_is_not_one_uac_overlapping() {
        const LB: &str = "10.0.0.9:5060";
        let f = hits(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, LB, BOB, "INVITE", "z9hG4bK-a1", 2, "at", "bt"),
                req(2_000, LB, ALICE, "INVITE", "z9hG4bK-b1", 2, "bt", "at"),
                rsp(3_000, BOB, LB, 491, "z9hG4bK-a1", 2, "at", "bt", ""),
                rsp(4_000, ALICE, LB, 491, "z9hG4bK-b1", 2, "bt", "at", ""),
            ],
        );
        assert!(f.is_empty(), "{f:?}");
    }

    /// Ordered keying must not blind the rule: a genuinely same-direction
    /// overlap still fires exactly once with the peer's reversed re-INVITE
    /// interleaved.
    #[test]
    fn a_same_direction_overlap_fires_amid_reversed_traffic() {
        const LB: &str = "10.0.0.9:5060";
        let f = hits(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, LB, ALICE, "INVITE", "z9hG4bK-b1", 2, "bt", "at"),
                req(2_000, LB, BOB, "INVITE", "z9hG4bK-a1", 2, "at", "bt"),
                req(3_000, LB, BOB, "INVITE", "z9hG4bK-a2", 3, "at", "bt"),
            ],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::OverlappingReInvite { branch, prior_branch, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((branch.as_str(), prior_branch.as_str()), ("z9hG4bK-a2", "z9hG4bK-a1"));
    }

    /// A retransmission reuses its own branch: the same request again, never a
    /// second one to overlap with.
    #[test]
    fn a_retransmitted_re_invite_does_not_overlap_itself() {
        assert!(hits(
            &NoReInviteWhileInviteInProgress,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
                req(2_000, ALICE, BOB, "INVITE", "z9hG4bK-1", 2, "at", "bt"),
            ],
        )
        .is_empty());
    }

    // ---- failed-reinvite-tears-down-dialog -------------------------------

    /// The final arrived: whatever it said, §14.1's prior state was reported
    /// on rather than silently dropped.
    #[test]
    fn a_re_invite_that_drew_a_final_is_compliant() {
        let f = eval(
            &FailedReinviteTearsDownDialog,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
                rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
                rsp(3_000, BOB, ALICE, 488, "z9hG4bK-re", 2, "at", "bt", ""),
            ],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The defect: a provisional and then silence on a dialog nothing ended —
    /// the prior session was destroyed by a failed re-INVITE.
    #[test]
    fn a_provisional_then_silence_is_violated() {
        let f = hits(
            &FailedReinviteTearsDownDialog,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
                rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
            ],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].emitter, ALICE, "the re-INVITE's sender is charged");
        let Decision::Violated(Evidence::AbandonedReInvite {
            branch, provisional_status, ..
        }) = &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((branch.as_str(), *provisional_status), ("z9hG4bK-re", 183));
    }

    /// "Absent another cause": a BYE on the dialog is an independent teardown
    /// that legitimately abandons the in-flight re-INVITE.
    #[test]
    fn a_byed_dialog_discharges_the_abandoned_re_invite() {
        assert!(hits(
            &FailedReinviteTearsDownDialog,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
                rsp(2_000, BOB, ALICE, 100, "z9hG4bK-re", 2, "at", "bt", ""),
                req(3_000, ALICE, BOB, "BYE", "z9hG4bK-bye", 3, "at", "bt"),
            ],
        )
        .is_empty());
    }

    /// An INITIAL INVITE abandoned after a 100 is the transaction layer's
    /// timeout, not a §14.1 prior-state breach: there is no prior state.
    #[test]
    fn an_abandoned_initial_invite_is_no_occasion() {
        assert!(eval(
            &FailedReinviteTearsDownDialog,
            &[
                req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-init", 1, "at", ""),
                rsp(2_000, BOB, ALICE, 100, "z9hG4bK-init", 1, "at", "", ""),
            ],
        )
        .is_empty());
    }

    /// A re-INVITE that drew NOTHING was never demonstrably live: §17.1.1.2's
    /// timeout owns it, so it is not an occasion here at all.
    #[test]
    fn a_re_invite_that_drew_no_provisional_is_no_occasion() {
        assert!(eval(
            &FailedReinviteTearsDownDialog,
            &[req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt")],
        )
        .is_empty());
    }

    /// An observation that stopped inside the final-response window shows
    /// truncation, not abandonment.
    #[test]
    fn silence_inside_the_final_window_is_undecidable() {
        let msgs = [
            req(1_000, ALICE, BOB, "INVITE", "z9hG4bK-re", 2, "at", "bt"),
            rsp(2_000, BOB, ALICE, 183, "z9hG4bK-re", 2, "at", "bt", ""),
        ];
        let open = eval_open(&FailedReinviteTearsDownDialog, &msgs);
        assert_eq!(open.len(), 1, "{open:?}");
        assert!(!open[0].decided(), "{:?}", open[0].decision);

        // An open observation that DID outlive the window decides.
        let mut long = msgs.to_vec();
        long.push(req(
            2_000 + REINVITE_FINAL_WINDOW_US,
            ALICE,
            BOB,
            "OPTIONS",
            "z9hG4bK-o",
            9,
            "at",
            "bt",
        ));
        let decided = eval_open(&FailedReinviteTearsDownDialog, &long);
        assert!(decided.iter().any(Finding::violated), "{decided:?}");
    }
}
