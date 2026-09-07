//! The ACK family of RFC 3261 §13 / §17.1.1.3 — whether the ACK CAME, and what
//! it CARRIED when it did.
//!
//! Whether it came is one wire fact read two ways, off one shared tracker:
//!
//!   - [`NoAckToDialogCreating2xx`] (§13.2.2.4) charges the UAC: it took the
//!     2xx that confirms its own dialog and owes the ACK. A later BYE is
//!     corroboration that the dialog died unconfirmed, never a discharge.
//!   - [`Unacked2xxNotCleared`] (§13.3.1.4) charges the UAS: its 2xx was never
//!     ACKed, so it must retransmit and, on giving up (64·T1), clear the
//!     dialog with a BYE. The ACK or any BYE on the dialog discharges it.
//!
//! The NON-2xx half is [`UnackedInviteNon2xxFinal`] (§17.1.1.3), and it keys
//! differently on purpose: that ACK belongs to the INVITE TRANSACTION and
//! reuses its branch, so it is owed hop by hop and read against the UAS that
//! sent the reject rather than against the dialog.
//!
//! What it carried is a comparison against the INVITE it acknowledges, paired
//! on the branch the two share (§17.1.1.3) — [`AckRequireSubsetOfInvite`] and
//! [`AckPreservesInviteRoute`], both charging the ACK's sender and both reading
//! the branch walk in [`super::branch`].
//!
//! **One ACK is owed per 2xx RECEIVED.** §13.2.2.4 puts the ACK in the UAC
//! CORE, not in the INVITE client transaction (§17.1.1.3), which is why a
//! retransmitted 2xx draws the ACK again and why the obligation is keyed by
//! the INVITE's CSeq NUMBER and the To tag. A fork answering the same INVITE
//! confirms its own dialog and owes its own ACK.
//!
//! **Dialog-creating, and nothing else** (UAC rule). The 2xx must answer an
//! INVITE the view carried WITHOUT a To tag, and must carry a To tag itself.
//! A re-INVITE's 2xx is answered inside a dialog that already exists — its
//! absence is the UAS rule's business, not this one's.
//!
//! **An obligation belongs to a dialog, not to a socket**, and **a hop is not
//! a UAC**: the ACK travels end to end, so an ACK ANY endpoint on the view
//! sent under the key is the obligation met, and the endpoint charged must be
//! the one whose own INVITE opened the dialog. The offence is an ABSENCE, so
//! each conservatism gate costs an occasion the population counts as
//! undecided rather than clean:
//!
//! - the dialog-creating INVITE must be witnessed at this vantage, or nothing
//!   says the 2xx creates a dialog at all (not even an occasion);
//! - the charged endpoint must have OPENED that INVITE rather than relayed
//!   it, and must not have passed the 2xx on;
//! - an ACK the emitter sent with no To tag makes every obligation of that
//!   emitter on this view ambiguous — it may be the one;
//! - the OBSERVATION must have kept running past the rule's window after the
//!   2xx, unless it is closed (then absence decides immediately).

use std::collections::{BTreeMap, BTreeSet};

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::branch::BranchReading;
use super::Obligation;

/// How long an open observation must have kept running after the 2xx before
/// its unanswered arrival is charged to the UAC.
///
/// One second. RFC 3261 §13.2.2.4 has the UAC core generate the ACK on
/// receipt, and §13.3.1.4 has the UAS start retransmitting the 2xx at T1
/// (500 ms) until one arrives — an ACK answering any rung of that ladder is
/// still an ACK. A capture that ran a full second past the 2xx and carries
/// none shows an absence; anything shorter would charge a recording that
/// stopped.
pub const ACK_WINDOW_US: u64 = 1_000_000;

/// How long an open observation must run past the UAS's 2xx before "neither
/// ACKed nor cleared" is charged to the UAS.
///
/// 33 seconds: §13.3.1.4 has the un-ACKed UAS give up at 64·T1 (32 s) and
/// then BYE, so only a capture that ran a second past that give-up point can
/// show the BYE never came.
pub const UNCLEARED_WINDOW_US: u64 = 33_000_000;

/// How long an open observation must run past a NON-2xx INVITE final before
/// "its ACK never arrived" is charged to the UAS that sent it.
///
/// 33 seconds: §17.2.1 has the rejecting UAS retransmit the final until Timer H
/// (64·T1, 32 s), and an ACK answering any rung of that ladder discharges the
/// transaction — only a capture that ran a second past the give-up point shows
/// none ever came.
pub const REJECT_ACK_WINDOW_US: u64 = 33_000_000;

/// §13.2.2.4 — the UAC's half. See the module doc.
pub struct NoAckToDialogCreating2xx;

impl Obligation for NoAckToDialogCreating2xx {
    fn id(&self) -> RuleId {
        RuleId::NoAckToDialogCreating2xx
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, f) in &seen.owed {
            let ObligationKey { party: uac, cseq, dialog } = *key;
            // A 2xx whose own INVITE this vantage never carried is not known
            // to create a dialog, so it is not an occasion of this rule at all.
            let Some(opened_us) = seen.invite_first_us.get(&cseq).copied() else { continue };
            let head = |decision| Finding {
                rule: RuleId::NoAckToDialogCreating2xx,
                emitter: uac.to_string(),
                taker: f.peer.to_string(),
                cseq,
                relayed: false, // a hop that forwarded the 2xx never reaches Violated
                anchor: f.msg,
                decision,
            };
            if seen.acked.contains(&(cseq, dialog)) {
                out.push(head(Decision::Compliant));
                continue;
            }
            // The two hop tests, each the other's blind spot: an INVITE this
            // endpoint did not open, and a 2xx it passed on.
            let opened_it =
                seen.invite.get(&(uac, cseq)).is_some_and(|at_us| *at_us == opened_us);
            let forwarded_it =
                seen.propagated.get(&(cseq, dialog)).is_some_and(|last| f.ts_us < *last);
            let window_us = wire.obs.last_us.saturating_sub(f.ts_us);
            let undecidable = if !opened_it {
                Some("the charged endpoint did not open the INVITE (a hop is not a UAC)")
            } else if forwarded_it {
                Some("the endpoint passed the 2xx on (a hop is not a UAC)")
            } else if seen.untagged_ack.contains(uac) {
                Some("an ACK the emitter sent names no dialog — it may be the one")
            } else if !wire.obs.absence_decidable(f.ts_us, ACK_WINDOW_US) {
                Some("the observation stopped inside the window — truncation, not absence")
            } else {
                None
            };
            if let Some(reason) = undecidable {
                out.push(head(Decision::Undecidable(reason)));
                continue;
            }
            let bye = seen.byes.iter().find(|b| b.ts_us > f.ts_us && b.on_dialog(dialog));
            out.push(head(Decision::Violated(Evidence::NoAck {
                final_msg: f.msg,
                final_hop: f.hop,
                final_ts_us: f.ts_us,
                to_tag: dialog.to_string(),
                status: f.status,
                retransmits: f.retransmits,
                window_us,
                emitter_window_us: wire.obs.last_seen(uac).saturating_sub(f.ts_us),
                bye_after_us: bye.map(|b| b.ts_us - f.ts_us),
                bye_by: bye.map(|b| b.src.to_string()),
            })));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// §13.3.1.4 — the UAS's half. See the module doc. Applies to EVERY 2xx to an
/// INVITE (a re-INVITE's 2xx is confirmed the same way); a forwarding hop's
/// emission is kept with `relayed: true` for consumer policy to weigh.
pub struct Unacked2xxNotCleared;

impl Obligation for Unacked2xxNotCleared {
    fn id(&self) -> RuleId {
        RuleId::Unacked2xxNotCleared
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, f) in &seen.emitted {
            let ObligationKey { party: uas, cseq, dialog } = *key;
            let head = |relayed, decision| Finding {
                rule: RuleId::Unacked2xxNotCleared,
                emitter: uas.to_string(),
                taker: f.peer.to_string(),
                cseq,
                relayed,
                anchor: f.msg,
                decision,
            };
            let relayed = seen
                .two_xx_first_us
                .get(&(cseq, dialog))
                .is_some_and(|first| *first < f.ts_us);
            let discharged = seen.acked.contains(&(cseq, dialog))
                || seen.byes.iter().any(|b| b.on_dialog(dialog));
            if discharged {
                out.push(head(relayed, Decision::Compliant));
                continue;
            }
            let undecidable = if seen.untagged_ack.contains(f.peer) {
                Some("an ACK the taker sent names no dialog — it may be the one")
            } else if !wire.obs.absence_decidable(f.ts_us, UNCLEARED_WINDOW_US) {
                Some("the observation stopped before the §13.3.1.4 give-up point")
            } else {
                None
            };
            if let Some(reason) = undecidable {
                out.push(head(relayed, Decision::Undecidable(reason)));
                continue;
            }
            out.push(head(
                relayed,
                Decision::Violated(Evidence::Uncleared {
                    final_msg: f.msg,
                    final_hop: f.hop,
                    final_ts_us: f.ts_us,
                    to_tag: dialog.to_string(),
                    status: f.status,
                    window_us: wire.obs.last_us.saturating_sub(f.ts_us),
                }),
            ));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §17.1.1.3 — a non-2xx INVITE final is ACKed, at the UAS that
/// sent it.** The ACK of a non-2xx final belongs to the INVITE transaction and
/// reuses its top-Via branch, so it is hop-by-hop: whatever the caller did, a
/// proxy in the path owes THIS UAS an ACK of its own. Without one the reject
/// retransmits to Timer H and its server transaction never completes — the
/// class a per-step expectation cannot see, because every step passed.
///
/// The occasion is ONE non-2xx final an endpoint SENT on a transaction it is
/// the UAS of — it TOOK that transaction's INVITE. Charges the sender; an ACK
/// on the same branch discharges it.
///
/// **A retransmitted final re-uses the obligation** (§17.2.1 obliges the
/// repeat, and one transaction owes one ACK), and **an ACK is only a discharge
/// once the obligation exists**: an ACK recorded before the final it answers
/// belongs to some earlier transaction on that branch, not this one.
///
/// **Only a branch this endpoint is the UAS of.** A UAC that TOOK a reject owes
/// nothing here — its ACK is the transaction layer's — and a 2xx is the other
/// two rules' business, since §13.2.2.4 puts that ACK in the UAC core instead.
pub struct UnackedInviteNon2xxFinal;

impl Obligation for UnackedInviteNon2xxFinal {
    fn id(&self) -> RuleId {
        RuleId::UnackedInviteNon2xxFinal
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per (UAS, call, branch): the INVITE it took, the reject it sent, and
        // whether the ACK came back. Built as the walk goes, so the order the
        // three arrived in is what decides.
        let mut invite: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut rejects: BTreeMap<(&str, &str, &str), Reject<'_>> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    invite
                        .entry((msg.dst.as_str(), msg.call_id.as_str(), branch))
                        .or_insert(mi);
                }
                // §17.1.1.3: the ACK of a non-2xx final rides its INVITE's own
                // branch, so it discharges that transaction and no other.
                Kind::Request { method } if method.eq_ignore_ascii_case("ACK") => {
                    if let Some(reject) =
                        rejects.get_mut(&(msg.dst.as_str(), msg.call_id.as_str(), branch))
                    {
                        reject.acked = true;
                    }
                }
                Kind::Response { status }
                    if (300..700).contains(status)
                        && msg.cseq_method.eq_ignore_ascii_case("INVITE") =>
                {
                    let key = (msg.src.as_str(), msg.call_id.as_str(), branch);
                    let Some(&invite_msg) = invite.get(&key) else { continue };
                    rejects.entry(key).or_insert(Reject {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        status: *status,
                        invite_msg,
                        taker: msg.dst.as_str(),
                        cseq: msg.cseq,
                        acked: false,
                    });
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (key, reject) in &rejects {
            let finding = |decision| Finding {
                rule: RuleId::UnackedInviteNon2xxFinal,
                emitter: key.0.to_string(),
                taker: reject.taker.to_string(),
                cseq: reject.cseq,
                relayed: false,
                anchor: reject.msg,
                decision,
            };
            if reject.acked {
                out.push(finding(Decision::Compliant));
            } else if !wire.obs.absence_decidable(reject.ts_us, REJECT_ACK_WINDOW_US) {
                out.push(finding(Decision::Undecidable(
                    "the observation stopped before the §17.2.1 Timer-H give-up point",
                )));
            } else {
                out.push(finding(Decision::Violated(Evidence::UnackedReject {
                    reject_msg: reject.msg,
                    reject_hop: reject.hop,
                    reject_ts_us: reject.ts_us,
                    status: reject.status,
                    invite_msg: reject.invite_msg,
                    branch: key.2.to_string(),
                    window_us: wire.obs.last_us.saturating_sub(reject.ts_us),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One non-2xx final a UAS sent, and whether its ACK came back.
#[derive(Debug)]
struct Reject<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    /// Index into the view's `msgs` of the INVITE it answers.
    invite_msg: usize,
    /// Where it went — the hop that owes the ACK.
    taker: &'a str,
    cseq: u32,
    acked: bool,
}

/// One ACK obligation: the party it is read against and the dialog it belongs
/// to. Spelled out as a struct so the three parts can never swap places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObligationKey<'a> {
    /// The UAC that took the 2xx (owed map) or the UAS that emitted it
    /// (emitted map).
    party: &'a str,
    cseq: u32,
    /// The confirmed dialog's To tag — a fork's 2xx confirms its own dialog
    /// and carries its own ACK, so the tag keeps two obligations apart.
    dialog: &'a str,
}

/// What one view's messages say about the ACK obligations on it — the shared
/// tracker both rules read.
#[derive(Debug, Default)]
struct Reading<'a> {
    /// (endpoint, INVITE CSeq number) → when that endpoint SENT the
    /// dialog-creating INVITE.
    invite: BTreeMap<(&'a str, u32), u64>,
    /// INVITE CSeq number → the first time a dialog-creating INVITE with that
    /// number crossed any vantage of the view. An endpoint whose own emission
    /// is not that first one relayed a dial somebody else opened.
    invite_first_us: BTreeMap<u32, u64>,
    /// The dialog-creating 2xx responses TAKEN, one entry per UAC obligation.
    owed: BTreeMap<ObligationKey<'a>, Final<'a>>,
    /// EVERY 2xx to an INVITE EMITTED (dialog-creating or not), one entry per
    /// UAS obligation.
    emitted: BTreeMap<ObligationKey<'a>, Final<'a>>,
    /// (INVITE CSeq number, To tag) some endpoint on this view ACKed. Keyed on
    /// the OBLIGATION rather than on who met it: the UAC's ACK travels end to
    /// end and may have gone straight past the hop this vantage watches.
    acked: BTreeSet<(u32, &'a str)>,
    /// (INVITE CSeq number, To tag) → the LAST time that 2xx crossed any
    /// vantage of the view. A taker that saw it before then passed it on.
    propagated: BTreeMap<(u32, &'a str), u64>,
    /// (INVITE CSeq number, To tag) → the FIRST time that 2xx crossed. An
    /// emitter whose own emission is later forwarded a 2xx somebody else
    /// originated.
    two_xx_first_us: BTreeMap<(u32, &'a str), u64>,
    /// An ACK this endpoint sent with no To tag: it names no dialog, so every
    /// obligation read against that endpoint becomes undecidable.
    untagged_ack: BTreeSet<&'a str>,
    /// Every BYE the view carried, in observation order — discharge for the
    /// UAS rule, corroboration for the UAC rule.
    byes: Vec<Bye<'a>>,
}

/// One 2xx as an obligation head: where it sat in the view and who the other
/// side of the obligation is.
#[derive(Debug)]
struct Final<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    /// Deliveries of this same 2xx to the same endpoint after the first.
    retransmits: u32,
    /// The endpoint on the other side of the obligation.
    peer: &'a str,
}

/// A BYE as this view carried it. Which dialog it belongs to is read off BOTH
/// tags, because either party may send it (RFC 3261 §15.1).
#[derive(Debug)]
struct Bye<'a> {
    ts_us: u64,
    src: &'a str,
    from_tag: Option<&'a str>,
    to_tag: Option<&'a str>,
}

impl Bye<'_> {
    fn on_dialog(&self, tag: &str) -> bool {
        self.from_tag == Some(tag) || self.to_tag == Some(tag)
    }
}

impl<'a> Reading<'a> {
    fn of(msgs: &'a [Msg]) -> Self {
        let mut seen = Reading::default();
        for (mi, msg) in msgs.iter().enumerate() {
            seen.absorb(mi, msg);
        }
        seen
    }

    /// Absorb one message.
    ///
    /// A repeat is not a fresh event for what CREATES an obligation, but it is
    /// evidence for what MEETS one: an ACK flagged as a repeat is still an ACK
    /// on the wire, and skipping it would charge a UAC whose first ACK this
    /// vantage simply missed.
    fn absorb(&mut self, mi: usize, msg: &'a Msg) {
        let fresh = !msg.repeat;
        if msg.is_request("ACK") {
            match msg.to_tag.as_deref() {
                Some(tag) => {
                    self.acked.insert((msg.cseq, tag));
                }
                None => {
                    self.untagged_ack.insert(msg.src.as_str());
                }
            }
        } else if msg.is_request("BYE") {
            self.byes.push(Bye {
                ts_us: msg.at_us,
                src: msg.src.as_str(),
                from_tag: msg.from_tag.as_deref(),
                to_tag: msg.to_tag.as_deref(),
            });
        } else if fresh && msg.is_request("INVITE") && msg.to_tag.is_none() {
            let first = self.invite_first_us.entry(msg.cseq).or_insert(msg.at_us);
            *first = (*first).min(msg.at_us);
            self.invite.entry((msg.src.as_str(), msg.cseq)).or_insert(msg.at_us);
        } else if let Some(status) = msg.status() {
            if !(200..300).contains(&status) || !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                return;
            }
            // A 2xx with no To tag confirms no dialog and draws no ACK.
            let Some(dialog) = msg.to_tag.as_deref() else { return };
            // Only a FRESH delivery says where the 2xx travelled: a
            // retransmission ladder to one endpoint would otherwise read as
            // that endpoint having passed the 2xx on.
            if fresh {
                let last = self.propagated.entry((msg.cseq, dialog)).or_insert(msg.at_us);
                *last = (*last).max(msg.at_us);
                let first = self.two_xx_first_us.entry((msg.cseq, dialog)).or_insert(msg.at_us);
                *first = (*first).min(msg.at_us);
            }
            let taken = ObligationKey { party: msg.dst.as_str(), cseq: msg.cseq, dialog };
            match self.owed.get_mut(&taken) {
                // The ladder collapses on ONE obligation (§13.2.2.4: one ACK
                // answers every retransmission); the rung count is what the
                // report carries instead.
                Some(seen) => seen.retransmits += 1,
                // A vantage whose first sight of a 2xx is already a repeat
                // never saw the obligation come due, so it records none.
                None if fresh => {
                    self.owed.insert(
                        taken,
                        Final {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            status,
                            retransmits: 0,
                            peer: msg.src.as_str(),
                        },
                    );
                }
                None => {}
            }
            // The UAS half: every fresh EMISSION of an INVITE 2xx opens the
            // emitter's clear-it obligation. Dialog-creating is NOT required —
            // §13.3.1.4 confirms a re-INVITE's 2xx the same way (the UAC half
            // above stays dialog-creating-only via its witnessed-INVITE gate).
            let emitted = ObligationKey { party: msg.src.as_str(), cseq: msg.cseq, dialog };
            if fresh {
                self.emitted.entry(emitted).or_insert(Final {
                    msg: mi,
                    hop: msg.hop,
                    ts_us: msg.at_us,
                    status,
                    retransmits: 0,
                    peer: msg.dst.as_str(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// What the ACK itself must CARRY — the branch-paired half of the family.
// ---------------------------------------------------------------------------

/// **RFC 3261 §13.2.2.4 — an ACK requires no more than its INVITE did.** An ACK
/// carries only option tags the INVITE it acknowledges already required: the
/// answering UAS agreed to an offer stated once, and a tag appearing first in
/// the ACK demands an extension of a transaction already negotiated, which a
/// real peer rejects.
///
/// **The occasion is one ACK an endpoint SENT on a branch it had already sent
/// the INVITE on** — the §17.1.1.3 branch reuse is what makes the two
/// comparable. An ACK whose INVITE this vantage never saw the same emitter send
/// is no occasion: there is nothing to be a subset OF.
///
/// **An ACK requiring nothing is the empty set, which is a subset**: it is a
/// met occasion, not a skipped one, so the report can say how rare the offence
/// is among the ACKs that could have carried it.
///
/// **An unreadable header block is `Undecidable`, never clean** — the tags are
/// read off the two messages' bytes.
///
/// Charges the ACK's sender.
pub struct AckRequireSubsetOfInvite;

impl Obligation for AckRequireSubsetOfInvite {
    fn id(&self) -> RuleId {
        RuleId::AckRequireSubsetOfInvite
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            let Some(invite) = branch.first_sent("INVITE") else { continue };
            for ack in branch.sent_of("ACK") {
                let finding = |decision| Finding {
                    rule: RuleId::AckRequireSubsetOfInvite,
                    emitter: key.emitter.to_string(),
                    taker: ack.taker.to_string(),
                    cseq: ack.cseq,
                    relayed: false,
                    anchor: ack.msg,
                    decision,
                };
                let (Some(ack_head), Some(invite_head)) =
                    (wire.msgs[ack.msg].head.as_deref(), wire.msgs[invite.msg].head.as_deref())
                else {
                    out.push(finding(Decision::Undecidable(
                        "no header block at this vantage",
                    )));
                    continue;
                };
                let ack_tags = sniff::option_tags(ack_head, "require");
                let invite_tags = sniff::option_tags(invite_head, "require");
                let extra_tags: Vec<String> = ack_tags
                    .iter()
                    .filter(|t| !invite_tags.iter().any(|i| i.eq_ignore_ascii_case(t)))
                    .cloned()
                    .collect();
                if extra_tags.is_empty() {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
                out.push(finding(Decision::Violated(Evidence::AckRequireNotSubset {
                    ack_require_msg: ack.msg,
                    ack_require_hop: ack.hop,
                    ack_require_ts_us: ack.ts_us,
                    invite_msg: invite.msg,
                    ack_tags,
                    invite_tags,
                    extra_tags,
                    branch: key.branch.to_string(),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// **RFC 3261 §17.1.1.3 — the ACK of a non-2xx final states its INVITE's
/// path.** That ACK belongs to the INVITE's own client transaction and reuses
/// its top-Via branch, so it walks the path the INVITE walked; a divergent
/// Route set sends it where no server transaction is waiting to absorb it.
///
/// **The occasion is one ACK an endpoint SENT on a branch it had already sent
/// the INVITE on.** That pairing is also what separates the two ACKs: the ACK
/// of a 2xx is a fresh transaction on its OWN branch (§13.2.2.4) whose route
/// set comes from the dialog, so it pairs with no INVITE here and is no
/// occasion of this rule at all.
///
/// **The comparison is the Route ROWS, in order** — a re-folded or reordered
/// set is a different path statement, and §17.1.1.3 asks for the INVITE's.
///
/// **An unreadable header block is `Undecidable`, never clean.**
///
/// Charges the ACK's sender.
pub struct AckPreservesInviteRoute;

impl Obligation for AckPreservesInviteRoute {
    fn id(&self) -> RuleId {
        RuleId::AckPreservesInviteRoute
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = BranchReading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, branch) in &seen.branches {
            let Some(invite) = branch.first_sent("INVITE") else { continue };
            for ack in branch.sent_of("ACK") {
                let finding = |decision| Finding {
                    rule: RuleId::AckPreservesInviteRoute,
                    emitter: key.emitter.to_string(),
                    taker: ack.taker.to_string(),
                    cseq: ack.cseq,
                    relayed: false,
                    anchor: ack.msg,
                    decision,
                };
                let (Some(ack_head), Some(invite_head)) =
                    (wire.msgs[ack.msg].head.as_deref(), wire.msgs[invite.msg].head.as_deref())
                else {
                    out.push(finding(Decision::Undecidable(
                        "no header block at this vantage",
                    )));
                    continue;
                };
                let ack_routes = sniff::header_values(ack_head, "route");
                let invite_routes = sniff::header_values(invite_head, "route");
                if ack_routes == invite_routes {
                    out.push(finding(Decision::Compliant));
                    continue;
                }
                out.push(finding(Decision::Violated(Evidence::AckRouteDiverged {
                    ack_route_msg: ack.msg,
                    ack_route_hop: ack.hop,
                    ack_route_ts_us: ack.ts_us,
                    invite_msg: invite.msg,
                    invite_hop: invite.hop,
                    invite_ts_us: invite.ts_us,
                    ack_routes,
                    invite_routes,
                    branch: key.branch.to_string(),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

#[cfg(test)]
mod tests {
    //! The pair's OWN semantics: what the capture conformance pins in
    //! `sip_pcap::rfc::ack` cannot state — the closed-observation collapse,
    //! the UAS half, and the two rules' opposite verdicts on one trace.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding, RuleId};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::{
        AckPreservesInviteRoute, AckRequireSubsetOfInvite, NoAckToDialogCreating2xx,
        Unacked2xxNotCleared, UnackedInviteNon2xxFinal,
    };

    const UAC: &str = "10.0.0.1:5060";
    const UAS: &str = "10.0.0.2:5060";

    fn msg(
        at_us: u64,
        src: &str,
        dst: &str,
        kind: Kind,
        cseq: u32,
        from_tag: Option<&str>,
        to_tag: Option<&str>,
    ) -> Msg {
        Msg {
            at_us,
            src: src.to_string(),
            dst: dst.to_string(),
            hop: 0,
            repeat: false,
            kind,
            call_id: "c1".to_string(),
            cseq,
            cseq_method: "INVITE".to_string(),
            via_branch: None,
            from_tag: from_tag.map(str::to_string),
            to_tag: to_tag.map(str::to_string),
            head: None,
            body: None,
        }
    }

    fn req(at_us: u64, src: &str, dst: &str, method: &str, cseq: u32, to_tag: Option<&str>) -> Msg {
        msg(at_us, src, dst, Kind::Request { method: method.to_string() }, cseq, Some("fa"), to_tag)
    }

    fn ok200(at_us: u64, cseq: u32) -> Msg {
        msg(at_us, UAS, UAC, Kind::Response { status: 200 }, cseq, Some("fa"), Some("tb"))
    }

    fn obs(msgs: &[Msg], closed: bool) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed }
    }

    /// An un-ACKed 2xx followed 10 ms later by end-of-stream: an OPEN
    /// observation cannot tell absence from truncation; a CLOSED one decides
    /// immediately, because the harness drained before stopping.
    #[test]
    fn a_closed_observation_collapses_the_ack_window() {
        let msgs =
            vec![req(1_000_000, UAC, UAS, "INVITE", 1, None), ok200(1_200_000, 1)];
        let open = obs(&msgs, false);
        let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &open });
        assert_eq!(f.len(), 1);
        assert!(
            matches!(f[0].decision, Decision::Undecidable(_)),
            "open: truncation, not absence: {:?}",
            f[0].decision
        );

        let closed = obs(&msgs, true);
        let f = NoAckToDialogCreating2xx.eval(&WireView { msgs: &msgs, obs: &closed });
        assert!(f[0].violated(), "closed: nothing was in flight: {:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the UAC owed the ACK");
    }

    /// D1, resolved (issue 29): one trace — a dialog-creating 2xx that is
    /// BYE'd but never ACKed — and the pair states BOTH obligations: the UAC
    /// broke §13.2.2.4 (a BYE is corroboration, never a discharge), and the
    /// UAS met §13.3.1.4 (the BYE is exactly its clearing duty).
    #[test]
    fn a_byed_but_unacked_2xx_charges_the_uac_and_clears_the_uas() {
        let msgs = vec![
            req(1_000_000, UAC, UAS, "INVITE", 1, None),
            ok200(1_200_000, 1),
            // The UAS gives up per §13.3.1.4 and BYEs its own dialog — its
            // minted tag rides From, the caller's rides To (tags swapped).
            msg(
                34_000_000,
                UAS,
                UAC,
                Kind::Request { method: "BYE".to_string() },
                9,
                Some("tb"),
                Some("fa"),
            ),
        ];
        let closed = obs(&msgs, true);
        let view = WireView { msgs: &msgs, obs: &closed };

        let uac_half = NoAckToDialogCreating2xx.eval(&view);
        assert_eq!(uac_half.len(), 1);
        assert!(uac_half[0].violated(), "{:?}", uac_half[0].decision);
        assert_eq!(uac_half[0].emitter, UAC);
        let Decision::Violated(Evidence::NoAck { bye_after_us, bye_by, .. }) =
            &uac_half[0].decision
        else {
            panic!("ack evidence: {:?}", uac_half[0].decision)
        };
        assert_eq!(*bye_after_us, Some(32_800_000), "the BYE corroborates, not discharges");
        assert_eq!(bye_by.as_deref(), Some(UAS));

        let uas_half = Unacked2xxNotCleared.eval(&view);
        assert_eq!(uas_half.len(), 1);
        assert!(
            matches!(uas_half[0].decision, Decision::Compliant),
            "the UAS cleared its dialog: {:?}",
            uas_half[0].decision
        );
        assert_eq!(uas_half[0].emitter, UAS);
    }

    /// The UAS half's violation: a 2xx the UAS neither saw ACKed nor cleared,
    /// with the observation closed — the silent answered-call leak.
    #[test]
    fn a_2xx_neither_acked_nor_byed_charges_the_uas_too() {
        let msgs =
            vec![req(1_000_000, UAC, UAS, "INVITE", 1, None), ok200(1_200_000, 1)];
        let closed = obs(&msgs, true);
        let f = Unacked2xxNotCleared.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, RuleId::Unacked2xxNotCleared);
        assert_eq!(f[0].emitter, UAS, "the UAS owed the clearing BYE");
        assert_eq!(f[0].taker, UAC);
        assert!(!f[0].relayed);
        assert!(f[0].violated(), "{:?}", f[0].decision);
    }

    /// A re-INVITE's 2xx is the UAS half's business too (§13.3.1.4 confirms it
    /// the same way) — while the UAC half stays dialog-creating-only.
    #[test]
    fn a_re_invites_unacked_2xx_is_the_uas_halfs_occasion_only() {
        let msgs = vec![
            req(1_000_000, UAC, UAS, "INVITE", 1, None),
            ok200(1_200_000, 1),
            req(1_250_000, UAC, UAS, "ACK", 1, Some("tb")),
            req(5_000_000, UAC, UAS, "INVITE", 2, Some("tb")),
            ok200(5_100_000, 2),
        ];
        let closed = obs(&msgs, true);
        let view = WireView { msgs: &msgs, obs: &closed };
        let uac_half = NoAckToDialogCreating2xx.eval(&view);
        assert_eq!(uac_half.len(), 1, "only the initial INVITE's 2xx: {uac_half:?}");
        assert!(matches!(uac_half[0].decision, Decision::Compliant));
        let uas_half = Unacked2xxNotCleared.eval(&view);
        assert_eq!(uas_half.len(), 2, "both 2xxs opened clearing obligations: {uas_half:?}");
        assert!(matches!(uas_half[0].decision, Decision::Compliant), "the initial was ACKed");
        assert!(uas_half[1].violated(), "the re-INVITE's was not: {:?}", uas_half[1].decision);
    }

    /// A hop that forwards a 2xx someone else originated carries the UAS
    /// half's obligation only as `relayed` — consumer policy's to weigh.
    #[test]
    fn a_forwarding_hops_uncleared_2xx_is_marked_relayed() {
        const HOP: &str = "10.0.0.9:5060";
        let msgs = vec![
            req(1_000_000, UAC, HOP, "INVITE", 1, None),
            req(1_010_000, HOP, UAS, "INVITE", 1, None),
            msg(1_200_000, UAS, HOP, Kind::Response { status: 200 }, 1, Some("fa"), Some("tb")),
            msg(1_210_000, HOP, UAC, Kind::Response { status: 200 }, 1, Some("fa"), Some("tb")),
        ];
        let closed = obs(&msgs, true);
        let f = Unacked2xxNotCleared.eval(&WireView { msgs: &msgs, obs: &closed });
        assert_eq!(f.len(), 2, "{f:?}");
        let by_emitter = |ep: &str| f.iter().find(|x| x.emitter == ep).unwrap();
        assert!(!by_emitter(UAS).relayed, "the far UAS originated its 2xx");
        assert!(by_emitter(HOP).relayed, "the hop passed it on");
    }

    // ---- what the ACK CARRIED (§13.2.2.4 / §17.1.1.3) --------------------

    /// A request the UAC SENT on `branch`, carrying `extra` header rows in the
    /// head the two carries rules read.
    fn on_branch(at_us: u64, method: &str, branch: &str, extra: &str) -> Msg {
        let head = format!(
            "{method} sip:bob@10.0.0.2 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch={branch}\r\n\
             From: <sip:alice@h>;tag=fa\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n\
             {extra}\r\n"
        );
        let mut m = req(at_us, UAC, UAS, method, 1, None);
        m.via_branch = Some(branch.to_string());
        m.head = Some(head.into_bytes());
        m
    }

    fn requires(msgs: &[Msg]) -> Vec<Finding> {
        AckRequireSubsetOfInvite.eval(&WireView { msgs, obs: &obs(msgs, true) })
    }

    fn routes(msgs: &[Msg]) -> Vec<Finding> {
        AckPreservesInviteRoute.eval(&WireView { msgs, obs: &obs(msgs, true) })
    }

    /// The obligation met: the ACK requires a tag the INVITE required.
    #[test]
    fn an_ack_requiring_what_its_invite_required_is_compliant() {
        let f = requires(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Require: 100rel\r\n"),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the ACK: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the sender of the ACK is charged");
        assert_eq!(f[0].taker, UAS);
    }

    /// The violation: the ACK escalates the transaction with a tag the INVITE
    /// never stated, and the evidence names exactly which.
    #[test]
    fn an_ack_requiring_more_than_its_invite_is_violated() {
        let f = requires(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", ""),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::AckRequireSubsetOfInvite);
        assert_eq!(f[0].anchor, 1, "the occasion rests on the ACK");
        let Decision::Violated(Evidence::AckRequireNotSubset { extra_tags, branch, .. }) =
            &f[0].decision
        else {
            panic!("require evidence: {:?}", f[0].decision)
        };
        assert_eq!(extra_tags.as_slice(), ["timer"]);
        assert_eq!(branch, "z9hG4bK-i");
    }

    /// An ACK requiring nothing is the empty set, which is a subset of
    /// anything: a MET occasion, so the report can rate the offence against
    /// the ACKs that could have carried it.
    #[test]
    fn an_ack_requiring_nothing_is_a_met_occasion() {
        let f = requires(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"),
            on_branch(3_000, "ACK", "z9hG4bK-i", ""),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The ACK of a 2xx rides its OWN branch, so it pairs with no INVITE and is
    /// no occasion of either carries rule — a different MUST governs it.
    #[test]
    fn an_ack_on_its_own_branch_is_no_occasion() {
        let msgs = [
            on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
            on_branch(3_000, "ACK", "z9hG4bK-a", "Require: timer\r\n"),
        ];
        assert!(requires(&msgs).is_empty(), "{:?}", requires(&msgs));
        assert!(routes(&msgs).is_empty(), "{:?}", routes(&msgs));
    }

    /// A vantage that carried no header block for one of the two settles
    /// nothing about what they said — UNDECIDABLE, never clean.
    #[test]
    fn a_carries_pair_without_header_bytes_is_undecidable() {
        let headless = |mut m: Msg| {
            m.head = None;
            m
        };
        let msgs = [
            headless(on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n")),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
        ];
        for f in [requires(&msgs), routes(&msgs)] {
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(
                matches!(f[0].decision, Decision::Undecidable("no header block at this vantage")),
                "{:?}",
                f[0].decision
            );
            assert!(!f[0].decided());
        }
    }

    /// A retransmitted ACK is the same act again: one occasion, tested by the
    /// copy that first left the sender.
    #[test]
    fn a_retransmitted_ack_is_not_a_second_occasion() {
        let again = |mut m: Msg| {
            m.repeat = true;
            m
        };
        let msgs = [
            on_branch(1_000, "INVITE", "z9hG4bK-i", ""),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Require: timer\r\n"),
            again(on_branch(4_000, "ACK", "z9hG4bK-i", "Require: timer\r\n")),
        ];
        assert_eq!(requires(&msgs).len(), 1, "{:?}", requires(&msgs));
        assert_eq!(routes(&msgs).len(), 1, "{:?}", routes(&msgs));
    }

    /// The obligation met: the non-2xx ACK walks the path its INVITE walked.
    #[test]
    fn an_ack_echoing_the_invites_route_is_compliant() {
        let f = routes(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the ACK: {f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UAC, "the sender of the ACK is charged");
    }

    /// The violation: the ACK drops the INVITE's Route set, so it is routed by
    /// a Request-URI no server transaction on that path is waiting on.
    #[test]
    fn an_ack_dropping_the_invites_route_is_violated() {
        let f = routes(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", "Route: <sip:p1@h;lr>\r\n"),
            on_branch(3_000, "ACK", "z9hG4bK-i", ""),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, RuleId::AckPreservesInviteRoute);
        let Decision::Violated(Evidence::AckRouteDiverged {
            ack_routes,
            invite_routes,
            branch,
            ..
        }) = &f[0].decision
        else {
            panic!("ack-route evidence: {:?}", f[0].decision)
        };
        assert!(ack_routes.is_empty());
        assert_eq!(invite_routes.as_slice(), ["<sip:p1@h;lr>"]);
        assert_eq!(branch, "z9hG4bK-i");
    }

    /// The Route ROWS are compared in order: the same hops re-folded into one
    /// row are a different path statement, and §17.1.1.3 asks for the
    /// INVITE's.
    #[test]
    fn ack_route_rows_are_compared_in_wire_order() {
        let two = "Route: <sip:p1@h;lr>\r\nRoute: <sip:p2@h;lr>\r\n";
        let folded = routes(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", two),
            on_branch(3_000, "ACK", "z9hG4bK-i", "Route: <sip:p1@h;lr>, <sip:p2@h;lr>\r\n"),
        ]);
        assert!(folded[0].violated(), "{:?}", folded[0].decision);
        let copied = routes(&[
            on_branch(1_000, "INVITE", "z9hG4bK-i", two),
            on_branch(3_000, "ACK", "z9hG4bK-i", two),
        ]);
        assert!(matches!(copied[0].decision, Decision::Compliant), "{:?}", copied[0].decision);
    }

    // ── UnackedInviteNon2xxFinal (RFC 3261 §17.1.1.3) ───────────────────────

    /// A message on a named branch between the two endpoints.
    fn on(at_us: u64, src: &str, dst: &str, kind: Kind, cseq: u32, branch: &str) -> Msg {
        let mut m = msg(at_us, src, dst, kind, cseq, Some("fa"), Some("tb"));
        m.via_branch = Some(branch.to_string());
        m
    }

    fn reject(at_us: u64, status: u16, branch: &str) -> Msg {
        on(at_us, UAS, UAC, Kind::Response { status }, 1, branch)
    }

    fn took(at_us: u64, method: &str, branch: &str) -> Msg {
        on(at_us, UAC, UAS, Kind::Request { method: method.to_string() }, 1, branch)
    }

    fn unacked(msgs: &[Msg]) -> Vec<Finding> {
        UnackedInviteNon2xxFinal.eval(&WireView { msgs, obs: &obs(msgs, true) })
    }

    /// §17.1.1.3: the ACK of a non-2xx final rides the INVITE's own branch, so
    /// it discharges that transaction — and a retransmitted final does not
    /// re-open it.
    #[test]
    fn an_ack_on_the_invite_branch_discharges_the_reject() {
        let f = unacked(&[
            took(1_000, "INVITE", "z9hG4bK-i"),
            reject(2_000, 486, "z9hG4bK-i"),
            reject(3_000, 486, "z9hG4bK-i"),
            took(4_000, "ACK", "z9hG4bK-i"),
        ]);
        assert_eq!(f.len(), 1, "one occasion, the transaction: {f:?}");
        assert_eq!(f[0].rule, RuleId::UnackedInviteNon2xxFinal);
        assert_eq!(f[0].emitter, UAS, "the UAS that sent the reject is charged");
        assert_eq!(f[0].taker, UAC);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    /// The defect: the reject went out and its mandatory ACK never came back.
    #[test]
    fn a_reject_that_is_never_acked_is_violated() {
        let f =
            unacked(&[took(1_000, "INVITE", "z9hG4bK-i"), reject(2_000, 486, "z9hG4bK-i")]);
        assert_eq!(f.len(), 1, "{f:?}");
        let Decision::Violated(Evidence::UnackedReject { status, branch, invite_msg, .. }) =
            &f[0].decision
        else {
            panic!("{:?}", f[0].decision)
        };
        assert_eq!((*status, branch.as_str(), *invite_msg), (486, "z9hG4bK-i", 0));
    }

    /// The obligation is the SENDER's: a UAC that merely TOOK a reject owes
    /// nothing here, and a 2xx is the dialog rules' business.
    #[test]
    fn a_taken_reject_and_a_2xx_open_no_occasion() {
        assert!(unacked(&[
            // The UAC's own side of a reject it received.
            on(1_000, UAC, UAS, Kind::Request { method: "INVITE".to_string() }, 1, "z9hG4bK-u"),
            on(2_000, UAS, UAC, Kind::Response { status: 486 }, 1, "z9hG4bK-x"),
            // A 2xx on a branch this UAS IS the server of.
            took(3_000, "INVITE", "z9hG4bK-i"),
            reject(4_000, 200, "z9hG4bK-i"),
        ])
        .is_empty());
    }

    /// An ACK recorded BEFORE the final it would answer belongs to an earlier
    /// transaction on that branch, not to the obligation this final opens.
    #[test]
    fn an_ack_preceding_the_reject_does_not_discharge_it() {
        let f = unacked(&[
            took(1_000, "INVITE", "z9hG4bK-i"),
            took(2_000, "ACK", "z9hG4bK-i"),
            reject(3_000, 486, "z9hG4bK-i"),
        ]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].violated(), "{:?}", f[0].decision);
    }

    /// An OPEN observation that stopped before the §17.2.1 give-up point shows
    /// truncation; a CLOSED one decides at once.
    #[test]
    fn a_closed_observation_collapses_the_reject_window() {
        let msgs =
            [took(1_000, "INVITE", "z9hG4bK-i"), reject(2_000, 486, "z9hG4bK-i")];
        let open = UnackedInviteNon2xxFinal
            .eval(&WireView { msgs: &msgs, obs: &obs(&msgs, false) });
        assert_eq!(open.len(), 1, "{open:?}");
        assert!(!open[0].decided(), "{:?}", open[0].decision);
        assert!(unacked(&msgs)[0].violated());
    }
}
