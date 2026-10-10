//! RFC 3261 §14 — an INVITE transaction INSIDE a confirmed dialog: how many may
//! be in flight, what answers one that races another, and how one that fails
//! must leave the dialog. THREE obligations, each read off one endpoint's own
//! view:
//!
//!   - [`NoReInviteWhileInviteInProgress`] (§14.1) charges the UAC: it holds
//!     one INVITE outstanding per dialog DIRECTION and waits for Confirmed.
//!   - [`ConcurrentReInvite500Or491`] (§14.2) charges the UAS: a re-INVITE that
//!     arrives while its sender's earlier INVITE is in progress is answered 500
//!     with `Retry-After`, and one crossing the other party's is answered 491.
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
//! **A spiral copy is never a second act either.** A request routed back
//! through a hop it already crossed (§16.3) reaches that hop again under a new
//! top Via, but every forwarder keeps the Vias below its own (§16.6 step 8), so
//! its bottom Via — sent-by and branch — is the one it left its originator
//! with, and its CSeq is unchanged (§16.6). An INVITE whose bottom Via and CSeq
//! are those of an INVITE in flight at the same endpoint is that INVITE by
//! another path: neither a race nor an overlap.
//!
//! **A 2xx does not end an INVITE transaction — the ACK does** (§14.1 rule 2,
//! RFC 6026's *Accepted* interval). A non-2xx final does end it there and then:
//! its ACK belongs to the transaction layer and proves nothing about the TU.
//!
//! **No top-Via branch, no occasion.** The branch is how an INVITE is paired
//! with its own answers (§17); without one nothing can say what is still in
//! flight, so the rules state nothing rather than guessing.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::header::SentBy;
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

/// **RFC 3261 §14.2 — a racing re-INVITE is answered 500 with Retry-After, or
/// 491 when it crosses.** A UAS serialises offer/answer: while one INVITE
/// transaction of a dialog is still in progress, a second one cannot be
/// evaluated against a session state that is itself mid-change. §14.2 has it
/// say so rather than answer both — the test UA answers both, so nothing else
/// catches it.
///
/// The occasion is ONE re-INVITE the endpoint TOOK while another INVITE of that
/// dialog was in progress at it. Charges the taker. When the pending INVITE
/// came from the same sender (same From tag), only 500 carrying `Retry-After`
/// discharges it (§14.2's first clause); when it came the other way, only 491
/// does (the second clause).
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
        // Each INVITE transaction's From tag: who sent it on the dialog.
        let mut sender_tag: BTreeMap<TxnKey<'_>, &str> = BTreeMap::new();
        let mut origins = Origins::default();
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
                    let spiral = origins.copies_one_of(msg, taker, in_flight);
                    origins.record(msg, txn);
                    // A Timer-A retransmission reuses its own branch.
                    if !in_flight.contains(branch) && !spiral {
                        let same_sender = |b: &&&str| {
                            let pending = TxnKey { branch: b, ..txn };
                            sender_tag.get(&pending) == Some(&from_tag)
                        };
                        let pending = in_flight
                            .iter()
                            .find(same_sender)
                            .map(|b| (*b, true))
                            .or_else(|| in_flight.iter().next().map(|b| (*b, false)));
                        if let Some((pending, same_sender)) = pending {
                            raced.push(Race {
                                txn,
                                msg: mi,
                                hop: msg.hop,
                                ts_us: msg.at_us,
                                cseq: msg.cseq,
                                sender: msg.src.as_str(),
                                branch,
                                pending_branch: pending,
                                same_sender,
                            });
                        }
                    }
                    in_flight.insert(branch);
                    dialog_of.insert(txn, dialog);
                    sender_tag.entry(txn).or_insert(from_tag);
                }
                Kind::Response { status } if msg.cseq_method.eq_ignore_ascii_case("INVITE") => {
                    let txn = TxnKey {
                        endpoint: msg.src.as_str(),
                        call_id: msg.call_id.as_str(),
                        branch,
                    };
                    if *status < 200 || *status >= 700 {
                        continue;
                    }
                    answered.entry(txn).or_insert(Answer {
                        status: *status,
                        // §14.2 makes `Retry-After` part of the 500 answer; a
                        // vantage carrying no header block cannot read it.
                        retry_after: msg
                            .head
                            .as_deref()
                            .map(|h| sniff::has_header(h, "retry-after")),
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
            if answer.status == 491 && !race.same_sender {
                out.push(finding(Decision::Compliant));
                continue;
            }
            let retry_after = match answer.retry_after {
                Some(present) => present,
                // Only a 500 turns on the header; anything else is already
                // decided by its status alone.
                None if answer.status == 500 => {
                    out.push(finding(Decision::Undecidable("no header block at this vantage")));
                    continue;
                }
                None => false,
            };
            if answer.status == 500 && retry_after && race.same_sender {
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
        let mut origins = Origins::default();
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
                        OrderedKey {
                            endpoint: sender,
                            call_id: msg.call_id.as_str(),
                            from_tag,
                            to_tag,
                        },
                        OrderedKey {
                            endpoint: sender,
                            call_id: msg.call_id.as_str(),
                            from_tag,
                            to_tag: "",
                        },
                    ] {
                        let Some(in_flight) = in_progress.get_mut(&dialog) else { continue };
                        in_flight.retain(|b| {
                            let txn = TxnKey {
                                endpoint: sender,
                                call_id: msg.call_id.as_str(),
                                branch: b,
                            };
                            let confirms = branch_cseq.get(&txn) == Some(&msg.cseq);
                            if confirms {
                                accepted.remove(&txn);
                            }
                            !confirms
                        });
                    }
                }
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
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
                    let spiral = origins.copies_one_of(msg, sender, in_flight);
                    origins.record(msg, txn);
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
                        // A Timer-A retransmission reuses its own branch, and
                        // a spiral copy its bottom Via: the same act again,
                        // never a second one.
                        let prior = if in_flight.contains(branch) || spiral {
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
                    let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
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
/// **"Absent another cause."** A BYE anywhere on that call at that endpoint
/// discharges the occasion; the violation is the re-INVITE that died on a
/// dialog nothing ever ended.
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
    /// The pending INVITE came from the same sender: §14.2's 500 case, not
    /// the crossing's 491.
    same_sender: bool,
}

/// The first final one transaction was answered with.
#[derive(Debug)]
struct Answer {
    status: u16,
    /// Whether it carried `Retry-After`, or `None` where the vantage carried no
    /// header block to read.
    retry_after: Option<bool>,
}

/// The bottom Via (sent-by and branch) and CSeq of each INVITE transaction an
/// endpoint drove, where the vantage carried them: what every forwarder keeps
/// (§16.6), so the INVITE by another path (§16.3) is told by them.
#[derive(Default)]
struct Origins<'a> {
    of: BTreeMap<TxnKey<'a>, (SentBy, String, u32)>,
}

impl<'a> Origins<'a> {
    fn record(&mut self, msg: &Msg, txn: TxnKey<'a>) {
        if let Some(origin) = origin(msg) {
            self.of.entry(txn).or_insert(origin);
        }
    }

    /// Whether `msg` is, by its bottom Via and CSeq, one of the INVITEs
    /// `endpoint` has `in_flight` on its dialog under another top-Via branch.
    fn copies_one_of(&self, msg: &Msg, endpoint: &str, in_flight: &BTreeSet<&str>) -> bool {
        let Some(origin) = origin(msg) else { return false };
        in_flight.iter().any(|branch| {
            let txn = TxnKey { endpoint, call_id: msg.call_id.as_str(), branch };
            self.of.get(&txn) == Some(&origin)
        })
    }
}

/// `msg`'s bottom Via sent-by and branch, and its CSeq number, or `None` where
/// the vantage carried no header block or the bottom Via names no branch.
fn origin(msg: &Msg) -> Option<(SentBy, String, u32)> {
    let bottom = msg.head.as_deref().and_then(sniff::bottom_via)?;
    Some((bottom.sent_by, bottom.branch?, msg.cseq))
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
mod tests;
