//! The **verdict inversion** a negative document earns (`PCAP2TEST_PIVOT_V3.md`
//! §11.2): a run that failed exactly as its document declared reports
//! `ok-negative`; one that failed any other way — including by not failing at
//! all — fails like any other run.
//!
//! **The inversion is the verdict's.** The gate still refuses the datagram, the
//! failure is still raised with its site and its evidence, and the recording is
//! still a faithful account of what crossed the wire. This module reads all
//! three AFTER the run and decides what they mean. Nothing here softens what the
//! run observed, and a run whose document declares nothing never reaches it.
//!
//! **A declaration is matched against the RECORDING, never against the wording
//! of a failure.** `unexpected-ack` names the ACK this platform sends to the
//! dialog-creating 2xx the ANCHOR STEP emitted, so the anchor's own emission
//! supplies the dialog — Call-ID, the INVITE CSeq number and the To-tag the
//! scripted peer minted — and only an ACK carrying all three, on the anchor's
//! own leg, that no flow step claimed, is the failure the document declared. An
//! unexpected datagram of any other shape, on any other leg, or in any other
//! dialog satisfies nothing.
//!
//! **The declaration is an INCLUSION, not an exhaustive prediction.** A negative
//! case replays a capture the replay is KNOWN to diverge from, and past the
//! divergence the rest of the tail diverges with it. So a run that produced
//! every declared failure tolerates the WIRE-class failures beside them — moved
//! to the verdict's `tolerated` list, never dropped — while every STRUCTURAL
//! failure still fails it. The two classes are `class` below, and the boundary
//! is not this module's to move: settle, postcondition, CDR and reap
//! verification are never deactivated.

use pivot_schema::bundle::{Arrived, DeclaredNote, Dir, Failure, RunVerdict, VerdictStatus};
use pivot_schema::must_fail::{DeclaredFailure, MustFail};
use sip_message::parser::custom::CustomParser;
use sip_message::{HeaderName, Method, SipMessage, SipParser};

use crate::gate::Inbound;
use crate::plan::Plan;
use crate::recording::Recording;

/// The dialog the anchor step's response rides, read off the datagram the run
/// actually emitted for it.
struct AnchorDialog {
    leg: String,
    call_id: String,
    cseq: u32,
    to_tag: Option<String>,
    /// The provisional's RSeq — what an `unexpected-prack`'s RAck must name.
    rseq: Option<u32>,
}

/// The failure a declaration predicts, as this run's recording pins it down: the
/// leg it arrived on, and the identity a verdict failure states it with.
struct Predicted {
    leg: String,
    arrived: Arrived,
}

/// Decide this run's verdict against what its document declared.
///
/// Every declaration the run produced moves out of `failures` and into
/// `must_fail`, carrying the failure the gate raised verbatim; a declaration the
/// RECORDING holds after the script ended is stated there too, since the gate it
/// would have tripped was no longer armed; every declaration the run did NOT
/// produce adds a failure of its own. The status is `ok-negative` exactly when
/// nothing is left in `failures`.
pub fn invert(verdict: &mut RunVerdict, plan: &Plan, recording: &Recording) {
    let declarations = &plan.document().must_fail;
    if declarations.is_empty() {
        return;
    }
    let mut notes = Vec::with_capacity(declarations.len());
    let mut absent = Vec::new();
    for declared in declarations {
        let mut note = DeclaredNote {
            failure: declared.failure,
            step: declared.step.clone(),
            derived_from: declared.derived_from,
            observed: None,
            recorded: None,
        };
        match predicted(plan, recording, declared) {
            Err(detail) => absent.push(Failure::DeclaredFailureNotProduced {
                declared: declared.failure,
                step: declared.step.clone(),
                detail,
            }),
            Ok(predicted) => match verdict.failures.iter().position(|f| names(f, &predicted)) {
                Some(index) => note.observed = Some(verdict.failures.remove(index)),
                // The datagram crossed the wire and no failure names it: the
                // script had ended, so nothing was armed to refuse it. The
                // declaration is matched against the RECORDING (§11.2), and the
                // recording holds it.
                None => note.recorded = Some(predicted.arrived.to_string()),
            },
        }
        notes.push(note);
    }
    verdict.must_fail = notes;
    if absent.is_empty() {
        tolerate_co_divergence(verdict, plan, declarations);
    } else {
        // A declaration nothing produced is a run that failed for another
        // reason, so nothing about it is tolerated: the reader gets the whole
        // list in `failures`, which is where a red run's evidence belongs.
        verdict.failures.extend(absent);
    }
    verdict.failed_step = verdict.failures.iter().find_map(Failure::step).map(str::to_string);
    verdict.status =
        if verdict.failures.is_empty() { VerdictStatus::OkNegative } else { VerdictStatus::Failed };
}

/// What a failure is EVIDENCE of, which is what decides whether a negative case
/// may carry it (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// The replay and the capture diverged on the wire. Past a declared
    /// divergence that is the expected consequence of it, so a negative case
    /// carries it.
    Wire,
    /// Something other than the wire did not hold: the run could not do its job,
    /// the call did not end, or an assertion about the system failed. A negative
    /// case never carries one.
    Structural,
}

/// The class of every failure. Exhaustive and wildcard-free ON PURPOSE: a new
/// [`Failure`] variant must be classified here deliberately, because the default
/// a wildcard would pick is the difference between a safeguard and a blind spot.
fn class(failure: &Failure) -> Class {
    match failure {
        // The wire family. A datagram nothing expected, a datagram no armed
        // expect matched, one after the flow ended, an expect the diverging tail
        // never satisfied, a ladder our platform paces differently and a dwell
        // its timers measure differently: every one of them is the replay and
        // the capture disagreeing about packets, which is what a declared
        // divergence makes expected.
        Failure::ExpectTimedOut { .. }
        | Failure::UnmatchedDatagram { .. }
        | Failure::UnexpectedDatagram { .. }
        | Failure::DatagramAfterFlow { .. }
        | Failure::RetransmitCountMismatch { .. }
        | Failure::TimingOutOfTolerance { .. } => Class::Wire,

        // The call did not end, was not billed, or did not settle — or a
        // transaction of it never completed. This project never deactivates
        // CDR and post-call cleanup verification, and a negative case owes its
        // call a teardown exactly like any other run.
        Failure::SettleTimedOut { .. }
        | Failure::FinalUnacknowledged { .. }
        | Failure::InviteUnanswered { .. }
        | Failure::BackgroundCount { .. }
        | Failure::CdrMismatch { .. }
        | Failure::FlowIncomplete { .. } => Class::Structural,

        // An assertion about the system, not about a packet's arrival. A check
        // that gates has already survived §9.1's lane scoping, so what is left
        // is a statement every lane holds the system to.
        Failure::CheckFailed { .. } => Class::Structural,

        // The run could not EMIT what the document states: a send that failed, a
        // deviation this interpreter cannot reproduce, a ladder it cannot pace,
        // an override it refuses, an emission that did not carry what it
        // promised. None of them is the capture and the replay disagreeing —
        // they are the replay never having spoken.
        Failure::SendFailed { .. }
        | Failure::DeviationUnimplemented { .. }
        | Failure::RetransmitLadderRefused { .. }
        | Failure::CseqOverrideRefused { .. }
        | Failure::EmissionNotPreserved { .. } => Class::Structural,

        // The document, the lane or the run's own machinery. A plan that does
        // not compile, an accessor that does not resolve, a missing injector, an
        // unbound identity, a misdirected call directive, a dead socket, a lost
        // recording, a stall, an unwound body (which is where the RFC gate's own
        // refusal lands).
        Failure::PlanRefused { .. }
        | Failure::AccessorUnresolved { .. }
        | Failure::InjectorMissing { .. }
        | Failure::IdentityUnbound { .. }
        | Failure::CallDirectiveUnplaced { .. }
        | Failure::TransportClosed { .. }
        | Failure::RecordingFailed { .. }
        | Failure::RunStalled { .. }
        | Failure::RunUnwound { .. } => Class::Structural,

        // A violation the SYSTEM UNDER TEST emits gates by §11.1, and a document
        // declaring a divergence is no licence to stop verifying the platform.
        Failure::RfcViolationUnverified { .. } => Class::Structural,

        // Never reaches this: it is raised BY the inversion, and the tolerance
        // runs only where none was. Classified anyway, and gating, so the
        // exhaustive match stays the whole vocabulary.
        Failure::DeclaredFailureNotProduced { .. } => Class::Structural,
    }
}

/// Move the co-occurring wire divergences out of `failures`, where the run
/// produced every declaration and every failure left is one a negative case may
/// carry.
///
/// All or nothing: a single structural failure leaves the list exactly as the
/// run wrote it, because a red run's `failures` must state everything that went
/// wrong rather than a filtered half of it.
fn tolerate_co_divergence(verdict: &mut RunVerdict, plan: &Plan, declarations: &[MustFail]) {
    let Some(from) = divergence(plan, declarations) else {
        return;
    };
    if verdict.failures.iter().all(|f| carried(f, plan, from)) {
        verdict.tolerated = std::mem::take(&mut verdict.failures);
    }
}

/// The flow position the run is known to diverge from: the EARLIEST anchor any
/// declaration names. Before it the replay was still following the capture, so a
/// failure there is a defect of its own and no consequence of the declaration.
fn divergence(plan: &Plan, declarations: &[MustFail]) -> Option<usize> {
    let mut earliest: Option<usize> = None;
    for declared in declarations {
        let order = plan.step(&declared.step)?.order;
        earliest = Some(earliest.map_or(order, |seen: usize| seen.min(order)));
    }
    earliest
}

/// Whether a negative case carries `failure`: wire class, and — where the
/// failure names a step — at or after the divergence.
///
/// A wire failure that names a step the plan does not hold cannot be placed
/// against the divergence at all, so it is not carried.
fn carried(failure: &Failure, plan: &Plan, from: usize) -> bool {
    if class(failure) != Class::Wire {
        return false;
    }
    match failure.step() {
        // Unplaced by construction: a stray datagram belongs to no step, and one
        // after the flow ended is after every step there is.
        None => true,
        Some(step) => plan.step(step).is_some_and(|s| s.order >= from),
    }
}

/// The dialog `declared`'s anchor opened on the wire this run.
fn anchor_dialog(
    plan: &Plan,
    recording: &Recording,
    declared: &MustFail,
) -> Result<AnchorDialog, String> {
    let step = plan
        .step(&declared.step)
        .ok_or_else(|| format!("step {:?} is no step of this flow", declared.step))?;
    if !step.is_send() {
        return Err(format!("step {:?} sends nothing, so this run emits no 2xx", declared.step));
    }
    let legs = recording.legs();
    let emitted = legs
        .get(&step.leg)
        .into_iter()
        .flatten()
        .filter(|m| m.dir == Dir::Out && m.step.as_deref() == Some(declared.step.as_str()))
        .find_map(|m| parse(m.wire()))
        .ok_or_else(|| {
            format!("step {:?} put no datagram on leg {:?} this run", declared.step, step.leg)
        })?;
    let SipMessage::Response(response) = &emitted else {
        return Err(format!(
            "step {:?} emitted a request, not the response the declaration is about",
            declared.step
        ));
    };
    let rseq = emitted.raw(HeaderName::RSeq).next().and_then(|v| v.trim().parse().ok());
    Ok(AnchorDialog {
        leg: step.leg.clone(),
        call_id: response.call_id().to_string(),
        cseq: response.cseq().seq(),
        to_tag: response.to().tag().map(str::to_string),
        rseq,
    })
}

/// The datagram `declared` predicts, where the recording holds one.
fn predicted(plan: &Plan, recording: &Recording, declared: &MustFail) -> Result<Predicted, String> {
    match declared.failure {
        DeclaredFailure::UnexpectedAck => {
            let anchor = anchor_dialog(plan, recording, declared)?;
            let legs = recording.legs();
            legs.get(&anchor.leg)
                .into_iter()
                .flatten()
                // Unclaimed and first of its kind: an ACK a step matched is one
                // the flow expected, and a repeat is not a second failure.
                .filter(|m| m.dir == Dir::In && m.step.is_none() && m.repeat_of.is_none())
                .filter_map(|m| parse(m.wire()))
                .map(|message| Inbound::of(&message))
                .find(|inbound| is_predicted_ack(&anchor, &anchor.leg, inbound))
                .map(|inbound| Predicted { leg: anchor.leg.clone(), arrived: inbound.arrived() })
                .ok_or_else(|| {
                    format!(
                        "leg {:?} holds no unclaimed ACK to the 2xx step {:?} emitted \
                         (call-id {}, CSeq {})",
                        anchor.leg, declared.step, anchor.call_id, anchor.cseq
                    )
                })
        }
        DeclaredFailure::UnexpectedPrack => {
            let anchor = anchor_dialog(plan, recording, declared)?;
            if anchor.rseq.is_none() {
                return Err(format!(
                    "step {:?} emitted no RSeq, so no PRACK acknowledges it",
                    declared.step
                ));
            }
            let legs = recording.legs();
            legs.get(&anchor.leg)
                .into_iter()
                .flatten()
                .filter(|m| m.dir == Dir::In && m.step.is_none() && m.repeat_of.is_none())
                .filter_map(|m| parse(m.wire()))
                .find(|message| is_predicted_prack(&anchor, message))
                .map(|message| Predicted {
                    leg: anchor.leg.clone(),
                    arrived: Inbound::of(&message).arrived(),
                })
                .ok_or_else(|| {
                    format!(
                        "leg {:?} holds no unclaimed PRACK to the reliable provisional step \
                         {:?} emitted (call-id {}, RSeq {:?})",
                        anchor.leg, declared.step, anchor.call_id, anchor.rseq
                    )
                })
        }
        DeclaredFailure::UnexpectedCancel => {
            let anchor = anchor_dialog(plan, recording, declared)?;
            let legs = recording.legs();
            legs.get(&anchor.leg)
                .into_iter()
                .flatten()
                .filter(|m| m.dir == Dir::In && m.step.is_none() && m.repeat_of.is_none())
                .filter_map(|m| parse(m.wire()))
                .map(|message| Inbound::of(&message))
                .find(|inbound| is_predicted_cancel(&anchor, inbound))
                .map(|inbound| Predicted { leg: anchor.leg.clone(), arrived: inbound.arrived() })
                .ok_or_else(|| {
                    format!(
                        "leg {:?} holds no unclaimed CANCEL for the transaction step {:?} \
                         answered (call-id {}, CSeq {})",
                        anchor.leg, declared.step, anchor.call_id, anchor.cseq
                    )
                })
        }
    }
}

/// Whether `inbound`, arriving on the anchor's leg, is the CANCEL for the
/// transaction the anchor's final answered: §9.1 has it name that INVITE by
/// Call-ID and CSeq NUMBER. No To-tag test — a CANCEL rides the INVITE's own
/// To, which the anchor's response tagged and the request never did.
fn is_predicted_cancel(anchor: &AnchorDialog, inbound: &Inbound) -> bool {
    inbound.method.as_deref() == Some("CANCEL")
        && inbound.call_id == anchor.call_id
        && inbound.cseq == anchor.cseq
}

/// Whether `message`, arriving on the anchor's leg, is the PRACK the anchor's
/// reliable provisional draws: its RAck names the provisional's RSeq and the
/// INVITE's CSeq (RFC 3262 §7.2), in the anchor's own dialog.
fn is_predicted_prack(anchor: &AnchorDialog, message: &SipMessage) -> bool {
    let SipMessage::Request(request) = message else { return false };
    let inbound = Inbound::of(message);
    if inbound.method.as_deref() != Some("PRACK") || inbound.call_id != anchor.call_id {
        return false;
    }
    matches!(&request.optional().rack, Ok(Some(rack))
        if Some(rack.rseq()) == anchor.rseq
            && rack.seq() == anchor.cseq
            && *rack.method() == Method::Invite)
}

/// Whether `inbound`, arriving on `leg`, is the ACK the anchor's 2xx draws.
fn is_predicted_ack(anchor: &AnchorDialog, leg: &str, inbound: &Inbound) -> bool {
    leg == anchor.leg
        && inbound.method.as_deref() == Some("ACK")
        && inbound.call_id == anchor.call_id
        && inbound.cseq == anchor.cseq
        && inbound.to_tag == anchor.to_tag
}

/// Whether a failure is the one the run raised for `predicted`. Three failures
/// can carry an arrival — no armed expect, no MATCHING armed expect, and one
/// after the flow finished — and every one of them names the leg and describes
/// what came, which is what the recording is compared against.
fn names(failure: &Failure, predicted: &Predicted) -> bool {
    let named = match failure {
        Failure::UnexpectedDatagram { leg, arrived, .. }
        | Failure::DatagramAfterFlow { leg, arrived }
        | Failure::UnmatchedDatagram { leg, arrived, .. } => (leg, arrived),
        _ => return false,
    };
    *named.0 == predicted.leg && *named.1 == predicted.arrived
}

fn parse(wire: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(wire).ok()
}

#[cfg(test)]
mod tests;
