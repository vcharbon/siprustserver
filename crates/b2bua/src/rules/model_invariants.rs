//! Model invariants: what the call record states at the end of every turn,
//! checked before the record lands (`router::interpret::process_result`) when
//! [`B2buaConfig::model_invariants`](crate::B2buaConfig) is on. A violation
//! panics: its first line names the invariant, the turn and the detail, then
//! the call's dump follows.
//!
//! [`INVARIANTS`] is the list: one line per invariant, its name and its check.
//! A check reads the record as the turn left it and, for the monotonic ones,
//! the record as the previous turn landed it (`None` for a call's first
//! landing).
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
//! To-tag past the ACK, and only the one-dialog half holds.

use std::fmt;
use std::sync::Mutex;

use b2bua_sdk::event::CallEvent;
use call::helpers::{find_leg, holds_retired, unacknowledged_relayed_provisionals, OwedPrack};
use call::{Call, Leg, LegDisposition, LegState, MessageDirection};
use sip_message::SipMessage;

/// The record before and after one turn.
pub struct Turn<'a> {
    /// The record as the previous turn landed it.
    pub before: Option<&'a Call>,
    /// The record this turn lands.
    pub after: &'a Call,
}

/// One invariant's check: `Err` carries the detail of the violation.
pub type Check = fn(&Turn<'_>) -> Result<(), String>;

/// Every model invariant, by name.
pub const INVARIANTS: &[(&str, Check)] = &[
    ("confirmed-a-leg-one-dialog", confirmed_a_leg_one_dialog),
    ("tag-map-names-a-dialog", tag_map_names_a_dialog),
    ("dialog-cseq-never-decreases", dialog_cseq_never_decreases),
    ("reliable-1xx-acknowledged-at-final", reliable_1xx_acknowledged_at_final),
    ("no-a-dialog-past-a-rejection", no_a_dialog_past_a_rejection),
    ("no-timer-on-a-terminated-leg", no_timer_on_a_terminated_leg),
];

/// Check every invariant on `turn`; panics on the first violated, naming
/// it and `label`, with the call's dump. The violation is also kept in the
/// process's record ([`violations`]): the dispatcher isolates a turn's
/// panic, so a harness reads the record to fail the run.
pub fn check(turn: &Turn<'_>, label: &(dyn fmt::Display + Sync)) {
    for (name, check) in INVARIANTS {
        if let Err(detail) = check(turn) {
            let violation = format!(
                "model invariant {name} violated after {label}: {detail}\n{:#?}",
                turn.after
            );
            VIOLATIONS.lock().unwrap_or_else(|e| e.into_inner()).push(violation.clone());
            panic!("{violation}");
        }
    }
}

/// Every violation [`check`] met in this process, in order. Process-wide:
/// exact under a runner that gives each test its own process.
static VIOLATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The violations [`check`] met in this process so far, each its panic
/// message.
pub fn violations() -> Vec<String> {
    VIOLATIONS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A confirmed a-leg holds exactly one dialog, and its local tag is the
/// To-tag of the 2xx the caller was sent (RFC 3261 §12.1.1, §13.3.1.4).
fn confirmed_a_leg_one_dialog(turn: &Turn<'_>) -> Result<(), String> {
    let a = &turn.after.a_leg;
    if a.state != LegState::Confirmed {
        return Ok(());
    }
    let [dialog] = a.dialogs.as_slice() else {
        return Err(format!(
            "the confirmed a-leg holds {} dialogs (local tags {:?})",
            a.dialogs.len(),
            a.dialogs.iter().map(|d| d.sip.local_tag.as_str()).collect::<Vec<_>>()
        ));
    };
    let tag = dialog.sip.local_tag.as_str();
    if let Some(answered) = &dialog.ext.answered_2xx {
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

/// Every a-facing tag mapping names, on each face, a live dialog or one the
/// answer retired: on the a-face an a-leg dialog of its a-tag, or a retired
/// one with relays still open; on the b-face a dialog of its b-tag on its
/// leg, a retired one with relays still open, or a losing fork the leg's
/// fork-straggler book keeps (RFC 3261 §12.1.2).
fn tag_map_names_a_dialog(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    for m in &call.tag_map {
        let a = &call.a_leg;
        let a_face =
            a.dialogs.iter().any(|d| d.sip.local_tag == m.a_tag) || holds_retired(a, &m.a_tag);
        let b_face = find_leg(call, &m.b_leg_id).is_some_and(|l| {
            let book = crate::rules::fork_straggler::Book::of(l);
            l.dialogs.iter().any(|d| d.sip.remote_tag == m.b_tag)
                || holds_retired(l, &m.b_tag)
                || book.forks.contains_key(&m.b_tag)
                || book.releases(&m.b_tag)
        });
        if !(a_face && b_face) {
            return Err(format!(
                "tag mapping {} -> {}:{} names no dialog (a-face held: {a_face}, b-face held: \
                 {b_face})",
                m.a_tag, m.b_leg_id, m.b_tag
            ));
        }
    }
    Ok(())
}

/// A dialog's local and remote CSeq never go down across a turn (RFC 3261
/// §12.2.1.1, §12.2.2), the dialog matched by leg and dialog ID.
fn dialog_cseq_never_decreases(turn: &Turn<'_>) -> Result<(), String> {
    let Some(before) = turn.before else { return Ok(()) };
    for leg in legs(turn.after) {
        let Some(old_leg) = find_leg(before, &leg.leg_id) else { continue };
        for d in &leg.dialogs {
            let id = (&d.sip.call_id, &d.sip.local_tag, &d.sip.remote_tag);
            let Some(old) = old_leg
                .dialogs
                .iter()
                .find(|o| (&o.sip.call_id, &o.sip.local_tag, &o.sip.remote_tag) == id)
            else {
                continue;
            };
            if d.sip.local_cseq < old.sip.local_cseq {
                return Err(format!(
                    "leg {} dialog {}/{}: local CSeq went from {} to {}",
                    leg.leg_id,
                    d.sip.local_tag,
                    d.sip.remote_tag,
                    old.sip.local_cseq,
                    d.sip.local_cseq
                ));
            }
            if let Some(was) = old.ext.remote_cseq {
                if d.ext.remote_cseq.is_none_or(|now| now < was) {
                    return Err(format!(
                        "leg {} dialog {}/{}: remote CSeq went from {was} to {:?}",
                        leg.leg_id, d.sip.local_tag, d.sip.remote_tag, d.ext.remote_cseq
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
                leg.leg_id, leg.state, leg.disposition, owed.rseq, owed.invite_cseq, owed.b_tag
            ));
        }
    }
    Ok(())
}

/// Whether `leg`'s INVITE of CSeq `invite_cseq` still awaits its final and
/// the leg is neither being cancelled nor ended: the leg's initial INVITE
/// while it is in setup, a relayed re-INVITE while its relay is open.
fn invite_open(leg: &Leg, invite_cseq: i64) -> bool {
    if matches!(leg.disposition, LegDisposition::Cancelling | LegDisposition::Rejected) {
        return false;
    }
    match leg.state {
        LegState::Trying | LegState::Early => true,
        LegState::Terminated => false,
        LegState::Confirmed => leg.dialogs.iter().any(|d| {
            d.ext
                .inbound_pending_requests
                .iter()
                .any(|p| p.method == "INVITE" && p.outbound_cseq == invite_cseq)
        }),
    }
}

/// Whether `owed` carried the offer of the dialog a 2xx confirmed on `leg`,
/// whose ACK is still awaited: the PRACK carries that ACK's answer.
fn deferred_to_ack(leg: &Leg, owed: &OwedPrack) -> bool {
    owed.offer.is_some()
        && leg
            .dialogs
            .iter()
            .any(|d| d.sip.remote_tag == owed.b_tag && d.ext.awaited_ack_cseq.is_some())
}

/// A non-2xx final sent on the caller's INVITE ends every a-dialog it could
/// have created (RFC 3261 §12.3, §13.2.2.4): the a-leg is neither early nor
/// confirmed past it.
fn no_a_dialog_past_a_rejection(turn: &Turn<'_>) -> Result<(), String> {
    let a = &turn.after.a_leg;
    match a.invite_final_sent {
        Some(status)
            if status >= 300 && matches!(a.state, LegState::Early | LegState::Confirmed) =>
        {
            Err(format!(
                "the caller's INVITE took {status} but the a-leg is {:?} with dialogs {:?}",
                a.state,
                a.dialogs.iter().map(|d| d.sip.local_tag.as_str()).collect::<Vec<_>>()
            ))
        }
        _ => Ok(()),
    }
}

/// No armed timer names a terminated leg.
fn no_timer_on_a_terminated_leg(turn: &Turn<'_>) -> Result<(), String> {
    let call = turn.after;
    for timer in &call.timers {
        let Some(leg_id) = timer.leg_id.as_deref() else { continue };
        if find_leg(call, leg_id).is_some_and(|l| l.state == LegState::Terminated) {
            return Err(format!(
                "timer {} ({:?}) is armed on terminated leg {leg_id}",
                timer.id, timer.timer_type
            ));
        }
    }
    Ok(())
}

fn legs(call: &Call) -> impl Iterator<Item = &Leg> {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter())
}

/// A one-line summary of the event a turn handled, for the violation's
/// first line.
pub struct EventSummary<'a> {
    pub call_ref: &'a str,
    pub event: &'a CallEvent,
}

impl fmt::Display for EventSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.event {
            CallEvent::Sip { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) => {
                    write!(f, "{} CSeq {} from {src}", r.method(), r.cseq().seq())?
                }
                SipMessage::Response(r) => write!(
                    f,
                    "{} to {} CSeq {} from {src}",
                    r.status(),
                    r.cseq().method(),
                    r.cseq().seq()
                )?,
            },
            CallEvent::Timer { timer_type, leg_id, .. } => {
                write!(f, "timer {timer_type:?} on {}", leg_id.as_deref().unwrap_or("the call"))?
            }
            CallEvent::Cancelled { invite_cseq, .. } => {
                write!(f, "CANCEL of INVITE CSeq {invite_cseq:?}")?
            }
            CallEvent::Timeout { method, leg_id, .. } => write!(
                f,
                "timeout of {} on {}",
                method.as_deref().unwrap_or("a request"),
                leg_id.as_deref().unwrap_or("the call")
            )?,
            CallEvent::InternalEvent { topic, outcome, .. } => {
                write!(f, "internal event {topic}/{outcome}")?
            }
            CallEvent::CallQuiesced { .. } => write!(f, "call quiesced")?,
        }
        write!(f, " on call {}", self.call_ref)
    }
}

#[cfg(test)]
#[path = "model_invariants_tests.rs"]
mod tests;
