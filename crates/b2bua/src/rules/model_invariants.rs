//! Model invariants: what the call record states at the end of every turn,
//! checked before the record lands (`router::interpret::process_result`) when
//! [`B2buaConfig::model_invariants`](crate::B2buaConfig) is on. A violation
//! panics: its first line names the invariant, the turn and the detail, then
//! the call's dump follows.
//!
//! [`INVARIANTS`] is the list: one line per invariant, its name and its check.
//! A check reads the record as the turn left it and, for the monotonic ones,
//! the record as the previous turn landed it (`None` for a call's first
//! landing); the answer check also reads the request the turn handled and
//! what the turn sends.
//!
//! Not covered: the turns that land a record without `process_result` — a
//! request refused before the rules (foreign dialog, stray CANCEL, CSeq below
//! the dialog's), the record created for an initial INVITE before its turn
//! runs, a reclaim's materialisation and the reaper's eviction. The next turn
//! of the call checks what they left.
//!
//! The a-dialog's tag is checked against the 2xx the caller was sent while
//! that 2xx awaits its ACK (`answered_2xx`), and afterwards against the
//! message ring's record of it; with the ring off nothing records the 2xx's
//! To-tag past the ACK, and only the one-live-dialog half holds.
//!
//! A timer dies with the scope it guards: no armed timer names a leg that is
//! Terminated or gone from the record.

use std::fmt;
use std::sync::Mutex;

use call::helpers::{
    callee_dialog, find_b_leg, find_leg, unacknowledged_relayed_provisionals, OwedPrack,
};
use call::{Call, DialogState, EndingBy, Leg, LegState, MessageDirection};
use sip_message::{Method, SipRequest};

use crate::effects::{OutboundBody, OutboundSipEffect};

/// The record before and after one turn, with the request the turn handled
/// and what it sent.
pub struct Turn<'a> {
    /// The record as the previous turn landed it.
    pub before: Option<&'a Call>,
    /// The record this turn lands.
    pub after: &'a Call,
    /// The request the turn handled, when its event is one.
    pub received: Option<&'a SipRequest>,
    /// What the turn sends.
    pub sent: &'a [OutboundSipEffect],
}

/// One invariant's check: `Err` carries the detail of the violation.
pub type Check = fn(&Turn<'_>) -> Result<(), String>;

/// Every model invariant, by name.
pub const INVARIANTS: &[(&str, Check)] = &[
    ("dialog-named-once", dialog_named_once),
    ("one-confirmed-dialog-per-leg", one_confirmed_dialog_per_leg),
    ("confirmed-a-leg-one-dialog", confirmed_a_leg_one_dialog),
    ("fronts-name-a-dialog", fronts_name_a_dialog),
    ("own-tag-names-a-dialog", own_tag_names_a_dialog),
    ("dialog-cseq-never-decreases", dialog_cseq_never_decreases),
    ("no-early-dialog-past-its-invite", no_early_dialog_past_its_invite),
    ("no-cancelled-dialog-past-its-invites-final", no_cancelled_dialog_past_its_invites_final),
    ("in-dialog-request-answered-or-relayed", in_dialog_request_answered_or_relayed),
    ("reliable-1xx-acknowledged-at-final", reliable_1xx_acknowledged_at_final),
    ("no-a-dialog-past-a-rejection", no_a_dialog_past_a_rejection),
    ("no-timer-on-a-terminated-leg", no_timer_on_a_terminated_leg),
];

/// Check every invariant on `turn`; panics on the first violated, naming
/// it and `label`. The violation is also kept in the process's record
/// ([`violations`]): the dispatcher isolates a turn's panic, so a harness
/// reads the record to fail the run. The first violation recorded since the
/// last [`mark`] carries the call's dump; a later one is its one line, kept
/// once per invariant and detail, at most [`KEPT_AFTER_FIRST`] of them. A
/// repeat of a violation already kept unwinds without the panic hook, so a
/// turn retried in a loop prints its violation once.
pub fn check(turn: &Turn<'_>, label: &(dyn fmt::Display + Sync)) {
    for (name, check) in INVARIANTS {
        if let Err(detail) = check(turn) {
            let head = format!("model invariant {name} violated after {label}: {detail}");
            match record(format!("{name}: {detail}"), head, || format!("{:#?}", turn.after)) {
                Kept::New(violation) => panic!("{violation}"),
                Kept::Repeat(head) => std::panic::resume_unwind(Box::new(head)),
            }
        }
    }
}

/// A violation as [`record`] kept it.
enum Kept {
    /// Not met since the mark: the message its panic states.
    New(String),
    /// Already met since the mark: its one line.
    Repeat(String),
}

/// How many one-line violations the record keeps past the first since a
/// [`mark`].
pub const KEPT_AFTER_FIRST: usize = 32;

/// The process's record of violations: each kept one as `(invariant and
/// detail, message)`, and where the last [`mark`] was taken.
struct Record {
    kept: Vec<(String, String)>,
    mark: usize,
}

/// Every violation [`check`] kept in this process, in order. Process-wide:
/// exact under a runner that gives each test its own process.
static VIOLATIONS: Mutex<Record> = Mutex::new(Record { kept: Vec::new(), mark: 0 });

/// Keep the violation `what` (its invariant and detail) headed `head`: with
/// the call's `dump` for the first since the mark, as `head` alone for a later
/// one, and not again for one already kept.
fn record(what: String, head: String, dump: impl FnOnce() -> String) -> Kept {
    let mut record = VIOLATIONS.lock().unwrap_or_else(|e| e.into_inner());
    let since = record.mark;
    if record.kept.len() == since {
        let message = format!("{head}\n{}", dump());
        record.kept.push((what, message.clone()));
        return Kept::New(message);
    }
    let later = &record.kept[since..];
    if later.iter().any(|(w, _)| *w == what) {
        return Kept::Repeat(head);
    }
    if later.len() <= KEPT_AFTER_FIRST {
        record.kept.push((what, head.clone()));
    }
    Kept::New(head)
}

/// A point in the process's record: the number of violations kept so far.
/// The first violation kept after it carries the call's dump.
pub fn mark() -> usize {
    let mut record = VIOLATIONS.lock().unwrap_or_else(|e| e.into_inner());
    record.mark = record.kept.len();
    record.mark
}

/// The violations [`check`] kept in this process so far, each its panic
/// message.
pub fn violations() -> Vec<String> {
    let record = VIOLATIONS.lock().unwrap_or_else(|e| e.into_inner());
    record.kept.iter().map(|(_, message)| message.clone()).collect()
}

/// Every dialog of a leg is reachable by exactly one name: its own tag is set
/// and no other dialog of the leg carries the same two tags (RFC 3261 §12).
fn dialog_named_once(turn: &Turn<'_>) -> Result<(), String> {
    for leg in legs(turn.after) {
        let mut seen = std::collections::HashSet::new();
        for d in &leg.dialogs {
            if d.our_tag.is_empty() {
                return Err(format!(
                    "leg {}: a dialog toward {} has no own tag",
                    leg.leg_id, d.peer_tag
                ));
            }
            if !seen.insert((d.our_tag.as_str(), d.peer_tag.as_str())) {
                return Err(format!(
                    "leg {}: two dialogs are named {}/{}",
                    leg.leg_id, d.our_tag, d.peer_tag
                ));
            }
        }
    }
    Ok(())
}

/// A leg's INVITE confirms one dialog (RFC 3261 §12.1, §13.2.2.4: the other
/// forks are released): no leg holds two dialogs a 2xx established and no
/// BYE completed on. The leg's own state reads off that dialog
/// ([`call::Leg::state`]).
fn one_confirmed_dialog_per_leg(turn: &Turn<'_>) -> Result<(), String> {
    for leg in legs(turn.after) {
        let confirmed: Vec<_> = leg.dialogs.iter().filter(|d| d.is_confirmed()).collect();
        if confirmed.len() > 1 {
            return Err(format!(
                "leg {} holds {} confirmed or ending dialogs {:?}",
                leg.leg_id,
                confirmed.len(),
                confirmed.iter().map(|d| (d.our_tag.as_str(), &d.state)).collect::<Vec<_>>()
            ));
        }
    }
    Ok(())
}

/// A confirmed a-leg holds exactly one live dialog — every other one the
/// answer retired — and its local tag is the To-tag of the 2xx the caller was
/// sent (RFC 3261 §12.1.1, §13.3.1.4).
fn confirmed_a_leg_one_dialog(turn: &Turn<'_>) -> Result<(), String> {
    let a = &turn.after.a_leg;
    if a.state() != LegState::Confirmed {
        return Ok(());
    }
    let live: Vec<_> = a.dialogs.iter().filter(|d| d.is_live()).collect();
    let [dialog] = live.as_slice() else {
        return Err(format!(
            "the confirmed a-leg holds {} live dialogs (local tags {:?})",
            live.len(),
            live.iter().map(|d| d.our_tag.as_str()).collect::<Vec<_>>()
        ));
    };
    let tag = dialog.our_tag.as_str();
    if let Some(answered) = &dialog.answered_2xx {
        if answered.dialog_tag != tag {
            return Err(format!(
                "the a-dialog's local tag {tag} is not the To-tag {} of the 2xx awaiting its ACK",
                answered.dialog_tag
            ));
        }
    }
    let invite_cseq = turn.after.a_leg_invite.cseq;
    let sent_2xx = a.messages.entries.iter().filter(|e| {
        e.direction != MessageDirection::Received
            && e.method == "INVITE"
            && e.cseq == invite_cseq
            && e.code.is_some_and(|c| (200..300).contains(&c))
    });
    for entry in sent_2xx {
        if let Some(to_tag) = entry.to_tag.as_deref().filter(|t| *t != tag) {
            return Err(format!(
                "the a-dialog's local tag {tag} is not the To-tag {to_tag} of the 2xx the caller \
                 was sent (ring seq {})",
                entry.seq
            ));
        }
    }
    Ok(())
}

/// Every callee dialog a caller-facing dialog fronts is a dialog of its leg,
/// live or retired (RFC 3261 §12.1.2).
fn fronts_name_a_dialog(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    for d in &call.a_leg.dialogs {
        for f in &d.fronts {
            let held = find_b_leg(call, &f.leg_id)
                .is_some_and(|l| callee_dialog(l, &f.peer_tag).is_some());
            if !held {
                return Err(format!(
                    "caller-facing dialog {} fronts {}:{}, which names no dialog",
                    d.our_tag, f.leg_id, f.peer_tag
                ));
            }
        }
    }
    Ok(())
}

/// The tag a leg answers its INVITE under ([`Leg::own_tag`]) names one of
/// its dialogs, and a leg whose INVITE this stack received names one as soon
/// as it holds a dialog.
fn own_tag_names_a_dialog(turn: &Turn<'_>) -> Result<(), String> {
    for leg in legs(turn.after) {
        match leg.own_tag.as_deref() {
            Some(own) if !leg.dialogs.iter().any(|d| d.our_tag == own) => {
                return Err(format!("leg {}: its own tag {own} names no dialog", leg.leg_id));
            }
            None if leg.outbound_invite.is_none() && !leg.dialogs.is_empty() => {
                return Err(format!("leg {} holds dialogs but no own tag", leg.leg_id));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A dialog stays on its leg until the call record is released, and its own
/// and peer CSeq never go down across a turn (RFC 3261 §12.2.1.1, §12.2.2),
/// the dialog matched by leg and its two tags.
fn dialog_cseq_never_decreases(turn: &Turn<'_>) -> Result<(), String> {
    let Some(before) = turn.before else { return Ok(()) };
    for old_leg in legs(before) {
        let Some(leg) = find_leg(turn.after, &old_leg.leg_id) else { continue };
        for old in &old_leg.dialogs {
            let Some(d) = leg.dialogs.by_tags(&old.our_tag, &old.peer_tag) else {
                return Err(format!(
                    "leg {} dialog {}/{} is gone",
                    leg.leg_id, old.our_tag, old.peer_tag
                ));
            };
            if d.our_cseq < old.our_cseq {
                return Err(format!(
                    "leg {} dialog {}/{}: local CSeq went from {} to {}",
                    leg.leg_id, d.our_tag, d.peer_tag, old.our_cseq, d.our_cseq
                ));
            }
            if let Some(was) = old.peer_cseq {
                if d.peer_cseq.is_none_or(|now| now < was) {
                    return Err(format!(
                        "leg {} dialog {}/{}: remote CSeq went from {was} to {:?}",
                        leg.leg_id, d.our_tag, d.peer_tag, d.peer_cseq
                    ));
                }
            }
        }
    }
    Ok(())
}

/// No relayed reliable provisional is left unacknowledged — PRACKed by
/// neither the party shown it nor this stack — once its INVITE has its final
/// or its leg is being cancelled or has ended (RFC 3262 §4), except an offer
/// it carried on the dialog a 2xx confirmed, whose PRACK carries the answer
/// of the ACK still awaited there (RFC 3264).
fn reliable_1xx_acknowledged_at_final(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    for leg in legs(call) {
        for owed in unacknowledged_relayed_provisionals(call, &leg.leg_id, None) {
            if invite_open(leg, owed.invite_cseq) || deferred_to_ack(leg, &owed) {
                continue;
            }
            return Err(format!(
                "leg {} ({:?}, {:?}): reliable 1xx RSeq {} of INVITE CSeq {} on dialog {} is \
                 un-PRACKed",
                leg.leg_id,
                leg.state(),
                leg.disposition(),
                owed.rseq,
                owed.invite_cseq,
                owed.b_tag
            ));
        }
    }
    Ok(())
}

/// Whether `leg`'s INVITE of CSeq `invite_cseq` still awaits its final and
/// the leg is not ended: the leg's initial INVITE while it is in setup, a
/// relayed re-INVITE while its relay is open on a confirmed leg.
fn invite_open(leg: &Leg, invite_cseq: i64) -> bool {
    match leg.state() {
        LegState::Trying | LegState::Early => true,
        LegState::Terminated => false,
        LegState::Confirmed => leg.dialogs.iter().any(|d| {
            d.inbound_pending_requests
                .iter()
                .any(|p| p.method == "INVITE" && p.outbound_cseq == invite_cseq)
        }),
    }
}

/// Whether `owed` carried the offer of the dialog a 2xx confirmed on `leg`,
/// whose ACK is still awaited: the PRACK carries that ACK's answer.
fn deferred_to_ack(leg: &Leg, owed: &OwedPrack) -> bool {
    owed.offer.is_some()
        && leg.dialogs.iter().any(|d| d.peer_tag == owed.b_tag && d.awaited_ack_cseq.is_some())
}

/// A non-2xx final sent on the caller's INVITE ends every a-dialog it could
/// have created (RFC 3261 §12.3, §13.2.2.4): none of her dialogs is early or
/// confirmed past it.
fn no_a_dialog_past_a_rejection(turn: &Turn<'_>) -> Result<(), String> {
    let a = &turn.after.a_leg;
    let Some(status) = a.invite_final_sent.filter(|s| *s >= 300) else { return Ok(()) };
    let open = |d: &&call::Dialog| matches!(d.state, DialogState::Early | DialogState::Confirmed);
    match a.dialogs.iter().find(open) {
        Some(d) => Err(format!(
            "the caller's INVITE took {status} but her dialog {} is {:?}",
            d.our_tag, d.state
        )),
        None => Ok(()),
    }
}

/// No dialog stays early once its INVITE is over (RFC 3261 §12.1, §12.1.2):
/// past that INVITE's final, on a leg a 2xx confirmed (the other forks
/// retired), and on a leg a BYE or this stack's CANCEL ended.
fn no_early_dialog_past_its_invite(turn: &Turn<'_>) -> Result<(), String> {
    for leg in legs(turn.after) {
        let over = match (leg.invite_final(), leg.state()) {
            (Some(status), _) => format!("answered {status}"),
            (None, LegState::Confirmed | LegState::Terminated) => format!("{:?}", leg.state()),
            (None, LegState::Trying | LegState::Early) => continue,
        };
        if let Some(d) = leg.dialogs.iter().find(|d| d.state == DialogState::Early) {
            return Err(format!(
                "leg {} ({over}) holds early dialog {}/{}",
                leg.leg_id, d.our_tag, d.peer_tag
            ));
        }
    }
    Ok(())
}

/// An early dialog this stack's CANCEL is ending ends on its INVITE's final
/// (RFC 3261 §9.1, §12.1): none outlives that final, and none rides an
/// INVITE no CANCEL was sent for.
fn no_cancelled_dialog_past_its_invites_final(turn: &Turn<'_>) -> Result<(), String> {
    for leg in legs(turn.after) {
        let cancelled = DialogState::Ending(EndingBy::Cancel);
        let Some(d) = leg.dialogs.iter().find(|d| d.state == cancelled) else { continue };
        let why = match (leg.invite_final(), leg.cancel_sent()) {
            (Some(status), _) => format!("its INVITE took {status}"),
            (None, false) => "no CANCEL of its INVITE was sent".to_string(),
            (None, true) => continue,
        };
        return Err(format!(
            "leg {}: dialog {}/{} is ending by CANCEL but {why}",
            leg.leg_id, d.our_tag, d.peer_tag
        ));
    }
    Ok(())
}

/// An in-dialog request the turn handled — an ACK aside, which takes no
/// answer (§17.1.1.3) — ends the turn answered or relayed: a final sent on
/// its transaction (§8.2.6), or its relay recorded on the dialog it went into,
/// whose final answers it there (§8.1.3.3).
fn in_dialog_request_answered_or_relayed(turn: &Turn<'_>) -> Result<(), String> {
    let Some(req) = turn.received else { return Ok(()) };
    let method = req.method();
    if req.to().tag().is_none() || matches!(method, Method::Ack | Method::Cancel) {
        return Ok(());
    }
    let cseq = req.cseq().seq();
    let branch = req.top_via().branch();
    let answered = turn.sent.iter().any(|e| match &e.body {
        OutboundBody::Response(r) => {
            r.status() >= 200
                && r.cseq().seq() == cseq
                && r.cseq().method() == *method
                && r.top_via().branch() == branch
        }
        _ => false,
    });
    let relayed = legs(turn.after)
        .flat_map(|l| l.dialogs.iter())
        .flat_map(|d| {
            let retired = d.retired().map(|r| r.open_relays.as_slice()).unwrap_or_default();
            d.inbound_pending_requests.iter().chain(retired)
        })
        .any(|p| {
            p.method.eq_ignore_ascii_case(method.as_str())
                && p.inbound_cseq == i64::from(cseq)
                && p.source_call_id == req.call_id().as_str()
        });
    if answered || relayed {
        return Ok(());
    }
    Err(format!("{method} CSeq {cseq} ends the turn neither answered nor relayed"))
}

/// A leg-scoped timer dies with its leg: no armed timer names a leg that is
/// Terminated or no longer in the record.
fn no_timer_on_a_terminated_leg(turn: &Turn<'_>) -> Result<(), String> {
    for timer in &turn.after.timers {
        let Some(leg_id) = timer.leg_id.as_deref() else { continue };
        let state = find_leg(turn.after, leg_id).map(Leg::state);
        if matches!(state, None | Some(LegState::Terminated)) {
            let state = state.map_or("gone".to_string(), |s| format!("{s:?}"));
            return Err(format!("timer {} is armed on leg {leg_id}, which is {state}", timer.id));
        }
    }
    Ok(())
}

fn legs(call: &Call) -> impl Iterator<Item = &Leg> {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter())
}

#[cfg(test)]
#[path = "model_invariants_tests.rs"]
mod tests;
