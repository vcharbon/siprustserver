//! RFC 3261 §12.2.2 — a request naming a dialog its taker never confirmed.

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::super::sides::Sides;
use super::super::Obligation;
use super::message_facts::remote_tag_of;

/// How long an OPEN observation must keep running past an in-dialog request
/// before "the taker never answered it" reads as silence rather than
/// truncation. One second — the §17.2 reasoning of
/// [`crate::rules::capability::ANSWER_WINDOW_US`]; in a CLOSED observation it
/// collapses.
pub const UNKNOWN_DIALOG_WINDOW_US: u64 = 1_000_000;

/// **§12.2.2 — an in-dialog request for an unknown dialog is answered 481.** A
/// request carrying both tags claims to ride a dialog; a UAS that has no such
/// dialog cannot serve it — the state the request assumes does not exist — and
/// says so with 481 Call/Transaction Does Not Exist. Serving it instead (the
/// test UA answers whatever it is handed) hides a stale or mis-copied dialog id
/// that a real UAS would refuse.
///
/// The occasion is ONE in-dialog request the endpoint TOOK on a call where this
/// vantage carried at least one dialog-CREATING response for it — without that
/// there is nothing to say which dialogs the endpoint has, and the request is
/// not an occasion at all. Charges the taker; the 481 discharges it, from
/// whichever socket the taker answers its transaction (`Sides`).
///
/// **A dialog is known by its PEER tag** — the dialog key the other four
/// rules use. It is the half the endpoint does not mint and the half that is
/// direction-independent, so one relay face that carried both directions of a
/// call knows both of its peers and is not charged for either. Naming the local
/// half instead would charge every such face.
///
/// **A non-2xx final to the establishing INVITE (sent without a To-tag) ends
/// the early dialog it rode (§12.3)**, so a request the UAS takes after sending
/// that final names no dialog. A dialog a 2xx accepted stays, and every other
/// tagged answer shows its dialog. A request is judged once, at its first
/// copy. A PRACK on the ended dialog is left to
/// [`crate::rules::prack::Prack2xxOr481`], which judges it by the `RSeq` it names. A
/// request the strict reading refuses ([`Msg::refused`]) owes its 400 instead
/// and is no occasion.
///
/// **ACK and CANCEL are not occasions.** An ACK elicits no response at all
/// (§17.1.1.3), so it cannot be answered 481; a CANCEL is matched by
/// transaction, not by dialog (§9.1).
pub struct UnknownDialog481;

impl Obligation for UnknownDialog481 {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per endpoint: the peer tags whose dialog it has seen CONFIRMED on a
        // call, and whether that call ever carried a dialog-creating response
        // for it at all. Built as the walk goes, so a request is judged against
        // what the endpoint knew BEFORE it.
        let mut confirmed: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        let mut creating_seen: BTreeSet<(&str, &str)> = BTreeSet::new();
        // The confirmed dialogs a 2xx to an INVITE accepted: no later non-2xx
        // final ends them.
        let mut accepted: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        // The early dialogs a non-2xx final ended.
        let mut ended: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        // `(Call-ID, branch)` of every INVITE sent without a To-tag: the
        // establishing transactions whose non-2xx final ends an early dialog.
        let mut establishing: BTreeSet<(&str, &str)> = BTreeSet::new();
        // Requests on an unknown dialog, awaiting the final the taker sent on
        // their transaction (0 = none yet).
        let mut unknown: Vec<usize> = Vec::new();
        let sides = Sides::of(wire.msgs);
        let mut answers: BTreeMap<(&str, &str, &str), u16> = BTreeMap::new();
        let mut out: Vec<Finding> = Vec::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let call = msg.call_id.as_str();
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { method } => {
                    let hop_by_hop =
                        method.eq_ignore_ascii_case("ACK") || method.eq_ignore_ascii_case("CANCEL");
                    let from_tag = msg.from_tag.as_deref().unwrap_or_default();
                    let to_tag = msg.to_tag.as_deref().unwrap_or_default();
                    if method.eq_ignore_ascii_case("INVITE") && to_tag.is_empty() {
                        if let Some(branch) = branch {
                            establishing.insert((call, branch));
                        }
                    }
                    // A retransmission is the same request again, judged at
                    // its first copy.
                    if msg.repeat
                        || msg.refused
                        || hop_by_hop
                        || from_tag.is_empty()
                        || to_tag.is_empty()
                        || call.is_empty()
                    {
                        continue;
                    }
                    let Some(branch) = branch else { continue };
                    let taker = msg.dst.as_str();
                    if !creating_seen.contains(&(taker, call)) {
                        continue; // nothing says which dialogs this endpoint has
                    }
                    if confirmed.contains(&(taker, call, from_tag)) {
                        out.push(finding_481(mi, msg, Decision::Compliant));
                        continue;
                    }
                    if method.eq_ignore_ascii_case("PRACK")
                        && ended.contains(&(taker, call, from_tag))
                    {
                        continue;
                    }
                    unknown.push(mi);
                    answers.entry((taker, call, branch)).or_insert(0);
                }
                Kind::Response { status } => {
                    // A tagged answer names both halves: the peer tag this
                    // endpoint learns, and the one it minted itself, and shows
                    // the dialog — except a non-2xx final on an establishing
                    // INVITE's branch, which ends the early dialog (§12.3)
                    // unless a 2xx accepted it.
                    let tagged = (200..300).contains(status)
                        || (*status > 100 && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty()));
                    let both = msg.from_tag.as_deref().is_some_and(|t| !t.is_empty())
                        && msg.to_tag.as_deref().is_some_and(|t| !t.is_empty());
                    let invite = msg.cseq_method.eq_ignore_ascii_case("INVITE");
                    let ends = *status >= 300
                        && invite
                        && branch.is_some_and(|b| establishing.contains(&(call, b)));
                    if tagged && both && !call.is_empty() {
                        for endpoint in [msg.src.as_str(), msg.dst.as_str()] {
                            let key =
                                (endpoint, call, remote_tag_of(msg, endpoint == msg.src.as_str()));
                            creating_seen.insert((endpoint, call));
                            if !ends {
                                confirmed.insert(key);
                                ended.remove(&key);
                                if invite && (200..300).contains(status) {
                                    accepted.insert(key);
                                }
                            } else if !accepted.contains(&key) {
                                confirmed.remove(&key);
                                ended.insert(key);
                            }
                        }
                    }
                    if *status >= 200 {
                        if let Some(branch) = branch {
                            if let Some(at) = answers.get_mut(&(sides.uas(msg), call, branch)) {
                                if *at == 0 {
                                    *at = *status;
                                }
                            }
                        }
                    }
                }
            }
        }

        for mi in unknown {
            let msg = &wire.msgs[mi];
            let branch = msg.via_branch.as_deref().unwrap_or_default();
            let answered = answers
                .get(&(msg.dst.as_str(), msg.call_id.as_str(), branch))
                .copied()
                .unwrap_or(0);
            let decision = if answered == 481 {
                Decision::Compliant
            } else if answered == 0
                && !wire.obs.absence_decidable(msg.at_us, UNKNOWN_DIALOG_WINDOW_US)
            {
                Decision::Undecidable(
                    "the observation stopped inside the window — truncation, not silence",
                )
            } else {
                let method = match &msg.kind {
                    Kind::Request { method } => method.as_str(),
                    Kind::Response { .. } => "",
                };
                Decision::Violated(Evidence::UnknownDialogRequest {
                    unknown_dialog_msg: mi,
                    unknown_dialog_hop: msg.hop,
                    unknown_dialog_ts_us: msg.at_us,
                    method: method.to_string(),
                    from_tag: msg.from_tag.clone().unwrap_or_default(),
                    to_tag: msg.to_tag.clone().unwrap_or_default(),
                    answered_status: answered,
                })
            };
            out.push(finding_481(mi, msg, decision));
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One §12.2.2 occasion, charged to the endpoint that TOOK the request — the
/// UAS that owed the 481 — with its sender as the taker of that answer.
fn finding_481(mi: usize, msg: &Msg, decision: Decision) -> Finding {
    Finding {
        rule: RuleId::UnknownDialog481,
        emitter: msg.dst.to_string(),
        taker: msg.src.to_string(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}
