//! `no-re-invite-while-invite-in-progress`: one INVITE outstanding per dialog
//! direction (RFC 3261 §14.1).

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use super::super::Obligation;
use super::{branch_msg, OrderedKey, Origins, TxnKey};

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
/// riding one — but it IS tracked: it stays outstanding until its own ACK, is
/// outstanding on each early dialog a To-tagged 1xx names until its final, and
/// re-keys onto the dialog id the moment its 2xx names one, so an INVITE on an
/// early dialog or overtaking that ACK still collides. A 2xx on an early dialog
/// to a later INVITE the capture never showed sent ends the filing there: the
/// capture lost that hop's final, and an unseen INVITE charges nothing.
pub struct NoReInviteWhileInviteInProgress;

impl Obligation for NoReInviteWhileInviteInProgress {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per requester (endpoint + dialog + DIRECTION): the branches of its own
        // INVITE transactions still outstanding.
        let mut in_progress: BTreeMap<OrderedKey<'_>, BTreeSet<&str>> = BTreeMap::new();
        let mut dialog_of: BTreeMap<TxnKey<'_>, OrderedKey<'_>> = BTreeMap::new();
        // The early dialogs a dialog-creating INVITE's To-tagged 1xx named: the
        // INVITE is outstanding on each of them until its final.
        let mut early_of: BTreeMap<TxnKey<'_>, BTreeSet<OrderedKey<'_>>> = BTreeMap::new();
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
                    if (101..200).contains(status)
                        && msg.cseq_method.eq_ignore_ascii_case("INVITE") =>
                {
                    let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                        continue;
                    };
                    let to_tag = msg.to_tag.as_deref().unwrap_or_default();
                    let uac = msg.dst.as_str();
                    let txn = TxnKey { endpoint: uac, call_id: msg.call_id.as_str(), branch };
                    // Only the dialog-creating INVITE, still open on its
                    // From-only key, gains an early dialog.
                    let Some(parked) = dialog_of.get(&txn).copied() else { continue };
                    let open = in_progress.get(&parked).is_some_and(|f| f.contains(branch));
                    if to_tag.is_empty() || !parked.to_tag.is_empty() || !open {
                        continue;
                    }
                    let early = OrderedKey { to_tag, ..parked };
                    in_progress.entry(early).or_default().insert(branch);
                    early_of.entry(txn).or_default().insert(early);
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
                    let Some(dialog) = dialog_of.get(&txn).copied() else {
                        // A 2xx to an INVITE this capture never saw sent: the
                        // capture lost that hop's traffic, and the dialog it
                        // names went on past any earlier INVITE filed there.
                        if *status < 300 {
                            let named = OrderedKey {
                                endpoint: uac,
                                call_id: msg.call_id.as_str(),
                                from_tag: msg.from_tag.as_deref().unwrap_or_default(),
                                to_tag: msg.to_tag.as_deref().unwrap_or_default(),
                            };
                            for (earlier, early) in early_of.iter_mut() {
                                if branch_cseq.get(earlier).is_some_and(|c| *c < msg.cseq)
                                    && early.remove(&named)
                                {
                                    if let Some(in_flight) = in_progress.get_mut(&named) {
                                        in_flight.remove(earlier.branch);
                                    }
                                }
                            }
                        }
                        continue;
                    };
                    // The final ends every early dialog the INVITE opened; a
                    // 2xx re-keys it onto the dialog it confirms below.
                    for early in early_of.remove(&txn).unwrap_or_default() {
                        if let Some(in_flight) = in_progress.get_mut(&early) {
                            in_flight.remove(branch);
                        }
                    }
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
