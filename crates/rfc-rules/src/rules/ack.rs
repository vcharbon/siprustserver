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
//! reuses its branch, so it is owed hop by hop — by the UAC that took the
//! reject, to the UAS that sent it — rather than read against the dialog.
//!
//! What it carried is a comparison against the INVITE it acknowledges, paired
//! on the branch the two share (§17.1.1.3) — [`AckRequireSubsetOfInvite`] and
//! [`AckPreservesInviteRoute`], both charging the ACK's sender and both reading
//! the branch walk in `super::branch`.
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
//! **A side of a transaction is named by the socket its INVITE used**
//! (`Sides`): the UAC by the socket the INVITE left from, wherever its sent-by
//! took the final; the UAS by the socket the INVITE reached, whichever socket
//! answered.
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
//!   it, and must not have passed the 2xx on — on its own call, or as a
//!   B2BUA onto another call whose caller never ACKed it, where its own ACK
//!   waits on that caller's (its legs tied by the sockets it holds on both,
//!   [`Parties::joined`](super::correlation::Parties::joined));
//! - an ACK the UAC's user agent sent on the call with no To tag, from any of
//!   its sockets, makes every obligation of that agent on the call ambiguous —
//!   it may be the one;
//! - the OBSERVATION must have kept running past the rule's window after the
//!   2xx, unless it is closed (then absence decides immediately).

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use super::branch::BranchReading;
use super::Obligation;

mod non_2xx;
mod reading;
pub use non_2xx::{UnackedInviteNon2xxFinal, REJECT_ACK_WINDOW_US};
use reading::{ObligationKey, Reading};

crate::registry::rules! {
    /// RFC 3261 §13.2.2.4: a UAC that took a dialog-creating 2xx to its own INVITE answers it with
    /// an ACK on that dialog. Charges the UAC; a BYE is corroboration that the dialog died
    /// unconfirmed, never a discharge.
    NoAckToDialogCreating2xx = "no-ack-to-dialog-creating-2xx";
    /// RFC 3261 §13.3.1.4: a UAS whose 2xx to an INVITE is never ACKed clears the dialog with a
    /// BYE. Charges the UAS; the ACK or any BYE on the dialog discharges it.
    Unacked2xxNotCleared = "unacked-2xx-not-cleared";
    /// RFC 3261 §13.2.2.4: an ACK requires only option tags the INVITE it acknowledges required.
    /// Charges the ACK's sender.
    AckRequireSubsetOfInvite = "ack-require-subset-of-invite";
    /// RFC 3261 §17.1.1.3: the ACK of a non-2xx final carries its INVITE's Route values — the two
    /// ride one transaction. Charges the ACK's sender.
    AckPreservesInviteRoute = "ack-preserves-invite-route";
    /// RFC 3261 §17.1.1.3: a non-2xx INVITE final is ACKed, and the ACK reaches the UAS that sent
    /// it. Charges the UAC that took the final and owed the ACK.
    UnackedInviteNon2xxFinal = "unacked-invite-non-2xx-final";
}

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

/// §13.2.2.4 — the UAC's half. See the module doc.
pub struct NoAckToDialogCreating2xx;

impl Obligation for NoAckToDialogCreating2xx {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, f) in &seen.owed {
            let ObligationKey { party: uac, call, cseq, dialog } = *key;
            // A 2xx whose own INVITE this vantage never carried is not known
            // to create a dialog, so it is not an occasion of this rule at all.
            let Some(opened_us) = seen.invite_first_us.get(&(call, cseq)).copied() else {
                continue;
            };
            let head = |decision| Finding {
                rule: RuleId::NoAckToDialogCreating2xx,
                emitter: uac.to_string(),
                taker: f.peer.to_string(),
                cseq,
                relayed: false, // a hop that forwarded the 2xx never reaches Violated
                anchor: f.msg,
                decision,
            };
            if seen.acked.contains(&(call, cseq, dialog)) {
                out.push(head(Decision::Compliant));
                continue;
            }
            // The two hop tests, each the other's blind spot: an INVITE this
            // endpoint did not open, and a 2xx it passed on.
            let opened_it =
                seen.invite.get(&(uac, call, cseq)).is_some_and(|at_us| *at_us == opened_us);
            let forwarded_it =
                seen.propagated.get(&(call, cseq, dialog)).is_some_and(|last| f.ts_us < *last);
            let relayed_unacked = seen
                .relayed_upstream([uac, f.took_at], call, f.ts_us)
                .is_some_and(|up| !seen.acked.contains(&(up.call, up.cseq, up.dialog)));
            let window_us = wire.obs.last_us.saturating_sub(f.ts_us);
            let undecidable = if !opened_it {
                Some("the charged endpoint did not open the INVITE (a hop is not a UAC)")
            } else if forwarded_it {
                Some("the endpoint passed the 2xx on (a hop is not a UAC)")
            } else if relayed_unacked {
                Some("the endpoint answered another call whose caller never ACKed (a B2BUA's ACK waits on its caller's)")
            } else if seen.untagged_ack.contains(&(f.uac_agent, call)) {
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
            let bye = seen.byes.iter().find(|b| b.ts_us > f.ts_us && b.on_dialog(call, dialog));
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
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let seen = Reading::of(wire.msgs);
        let mut out = Vec::new();
        for (key, f) in &seen.emitted {
            let ObligationKey { party: uas, call, cseq, dialog } = *key;
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
                .get(&(call, cseq, dialog))
                .is_some_and(|first| *first < f.ts_us);
            let discharged = seen.acked.contains(&(call, cseq, dialog))
                || seen.byes.iter().any(|b| b.on_dialog(call, dialog));
            if discharged {
                out.push(head(relayed, Decision::Compliant));
                continue;
            }
            let undecidable = if seen.untagged_ack.contains(&(f.uac_agent, call)) {
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
                    out.push(finding(Decision::Undecidable("no header block at this vantage")));
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
                    out.push(finding(Decision::Undecidable("no header block at this vantage")));
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
mod tests;
