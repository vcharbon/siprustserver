//! What an arrival **no armed expect matched** does to the run
//! (`PCAP2TEST_PIVOT_V3.md` §14): which step it is charged to, what it
//! retires, and whether the run can still **go on** (§11.2).
//!
//! **A response is charged to the expect of its own transaction.** A response
//! rides the client transaction of the request it answers, identified by CSeq
//! method and number (RFC 3261 §17.1.3, §8.1.3.1); a leg awaiting a response is
//! the client of that transaction, so the transaction an expect gated on
//! `status` plus `cseq-method` waits on is the one the leg's own send of that
//! method opened — read off the leg's RECORDING, never off the step's captured
//! `cseq` (§6.3). Among the armed expects on the datagram's transaction the
//! charge lands on the one naming the very status that arrived, else the first
//! REQUIRED one in leg order, else the first `optional`; only then does the
//! status say whether it is a substitution.
//! With no armed expect on that transaction the charge falls to the closest
//! step: one whose discriminator matches, else the first armed.
//!
//! **A final retires the expect it was charged on.** RFC 3261 §17.1.3 ends a
//! client transaction at its final: no response of another status rides it
//! again, so a required expect charged with a final of another status can
//! never be satisfied and is RETIRED — the charge already made, no second one
//! at its budget. An `optional` so charged is released, and so is every
//! tolerated absence armed on the ended transaction. A block member (`alt`
//! branch, `unordered` member) is charged but never retired: the block keeps
//! its all-or-none rule below.
//!
//! **Going on is a dialog fact, not a transaction one.** After a retirement the
//! script ends only where the tail is no longer composable: an INVITE final of
//! the other class than the retired step named (2xx against non-2xx) on a leg
//! whose dialog is NOT yet confirmed — the initial INVITE's final decides
//! whether the dialog the tail was scripted for exists (§12.1, §13.2.2.3),
//! while a re-INVITE's final of either class leaves it as it was — or a final
//! where the retired step named a PROVISIONAL, the tail having been scripted
//! for a transaction still open. Every other retirement goes on.
//!
//! **Where nothing is retired, all the alternatives or none.** `alt` branches
//! and `unordered` members arm several expects on one leg at once, and the run
//! can still go on while any one of them can still be satisfied — so such a
//! leg blocks only when the arrival contradicts EVERY required expect armed on
//! it. A provisional ends nothing, a request ends nothing, and a final for
//! another transaction ends nothing.

use pivot_schema::bundle::{Dir, RecordedMessage};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::gate::{self, Inbound};
use crate::plan::{CompiledStep, Discriminator};

/// One armed expect, with what the cursor knows about its place.
#[derive(Debug, Clone)]
pub struct Armed<'a> {
    pub step: &'a CompiledStep,
    /// The send step that opened the transaction the expect waits on, where
    /// the leg sent it (`Cursor::opening_send`); `None` on a relay of another
    /// leg's origination and on a status-only expect.
    pub opener: Option<&'a str>,
    /// Whether the step may be retired: a message item's own expect, never a
    /// block member.
    pub retirable: bool,
}

/// What one unmatched arrival does to the run.
#[derive(Debug, Clone)]
pub struct Unmatched<'a> {
    /// The step the failure is charged to.
    pub charged: &'a CompiledStep,
    /// Whether `charged` waits on the very transaction the datagram rides.
    pub own_transaction: bool,
    /// The required expect the final retired, where it did.
    pub retired: Vec<String>,
    /// The tolerated absences released on the ended transaction.
    pub released: Vec<String>,
    /// Whether the script ends here (§11.2).
    pub ends: bool,
}

/// Charge, retire and decide for `inbound` — an arrival every armed expect on
/// its leg refused — against `armed`, the expects the cursor has armed there,
/// and `leg`, that leg's recording so far in wire order. `confirmed` is whether
/// an INVITE 2xx had confirmed the leg's dialog BEFORE this arrival. `None`
/// with nothing armed.
pub fn unmatched<'a>(
    inbound: &Inbound,
    armed: &[Armed<'a>],
    leg: &[RecordedMessage],
    confirmed: bool,
) -> Option<Unmatched<'a>> {
    let first = armed.first()?;
    let steps: Vec<CompiledStep> = armed.iter().map(|a| a.step.clone()).collect();
    let method = Method::from_wire(&inbound.cseq_method);
    let mut candidates: Vec<&Armed<'a>> = armed
        .iter()
        .filter(|a| {
            inbound.status.is_some() && awaited(a, leg) == Some((method.clone(), inbound.cseq))
        })
        .collect();
    candidates.sort_by_key(|a| a.step.order);
    // The candidate naming the very status that arrived is the step beyond
    // doubt (content or an early dialog refused it); among the others the first
    // required one in leg order, else the first tolerated absence.
    let Some(charged) = candidates
        .iter()
        .find(|a| gate::discriminates(a.step, inbound).matches())
        .or_else(|| candidates.iter().find(|a| !a.step.optional_expect()))
        .or_else(|| candidates.first())
        .copied()
    else {
        // Nobody's: diagnosed against the closest step, and the leg blocks only
        // where the arrival contradicts every required expect armed on it.
        let closest = armed
            .iter()
            .find(|a| gate::discriminates(a.step, inbound).matches())
            .unwrap_or(first)
            .step;
        return Some(Unmatched {
            charged: closest,
            own_transaction: false,
            retired: Vec::new(),
            released: Vec::new(),
            ends: contradicts_every_required(inbound, &steps, leg),
        });
    };
    let mut out = Unmatched {
        charged: charged.step,
        own_transaction: true,
        retired: Vec::new(),
        released: Vec::new(),
        ends: false,
    };
    let (Some(status), Some(named)) = (inbound.status, names(charged.step)) else {
        return Some(out);
    };
    // A provisional ends nothing, and the status the step names contradicts
    // nothing: what refused it was content or an early dialog, and another copy
    // of it can still satisfy the step.
    if status < 200 || named == status {
        return Some(out);
    }
    // A final ended the transaction (§17.1.3): every tolerated absence armed on
    // it is released, and the required expect charged with it is retired.
    out.released =
        candidates.iter().filter(|a| a.step.optional_expect()).map(|a| a.step.id.clone()).collect();
    if charged.step.optional_expect() {
        return Some(out);
    }
    if !charged.retirable {
        out.ends = contradicts_every_required(inbound, &steps, leg);
        return Some(out);
    }
    out.retired.push(charged.step.id.clone());
    let class_changed = method == Method::Invite && (named >= 300) != (status >= 300);
    out.ends = named < 200 || (class_changed && !confirmed);
    Some(out)
}

/// The status an expect names on a response, where it names one.
fn names(step: &CompiledStep) -> Option<u16> {
    match &step.discriminator {
        Discriminator::Response { status, .. } if step.is_expect() => Some(*status),
        _ => None,
    }
}

/// The transaction an armed expect waits on — CSeq method and number (RFC 3261
/// §17.1.3) — read off the leg's recording: the request sent under the step
/// that opened it, else the leg's last send of the method.
fn awaited(armed: &Armed<'_>, leg: &[RecordedMessage]) -> Option<(Method, u32)> {
    let Discriminator::Response { cseq_method: Some(method), .. } = &armed.step.discriminator
    else {
        return None;
    };
    let method = Method::from_wire(method);
    let cseq = match armed.opener {
        Some(opener) => sent_under(leg, opener),
        None => last_sent(leg, &method),
    }?;
    Some((method, cseq))
}

/// The CSeq of the request this leg sent under `step`, where it is recorded.
fn sent_under(leg: &[RecordedMessage], step: &str) -> Option<u32> {
    leg.iter()
        .filter(|recorded| recorded.dir == Dir::Out && recorded.step.as_deref() == Some(step))
        .find_map(|recorded| match CustomParser::new().parse(recorded.wire()).ok()? {
            SipMessage::Request(request) => Some(request.cseq().seq()),
            _ => None,
        })
}

/// Whether a final leaves EVERY required armed expect unsatisfiable: the
/// all-or-none rule of a block, and of a leg where nothing was retired.
fn contradicts_every_required(
    inbound: &Inbound,
    armed: &[CompiledStep],
    leg: &[RecordedMessage],
) -> bool {
    // Only a final response ends a transaction (RFC 3261 §17.1). A request and a
    // provisional leave every armed expect exactly as satisfiable as it was.
    let Some(status) = inbound.status.filter(|status| *status >= 200) else {
        return false;
    };
    // A tolerated absence is released, not failed, so it can never be what wedges
    // a leg: only the expects the document REQUIRES can block.
    let mut required = armed.iter().filter(|step| !step.optional_expect()).peekable();
    if required.peek().is_none() {
        return false;
    }
    let sent = last_sent(leg, &Method::from_wire(&inbound.cseq_method));
    required.all(|step| contradicts(step, status, inbound, sent))
}

/// Whether a final response leaves one armed expect unsatisfiable: the expect
/// waits for a RESPONSE naming the method of the transaction this final
/// answered, that transaction is the one the leg has outstanding for the
/// method, and the status it waits for is not the status that arrived. A
/// same-status arrival contradicts nothing — a forking INVITE draws a 2xx per
/// answered fork (§13.2.2.4) and a final is retransmitted — so what refused it
/// was content or an early dialog, and another copy can still satisfy the step.
fn contradicts(step: &CompiledStep, status: u16, inbound: &Inbound, sent: Option<u32>) -> bool {
    // An expect gated on a status alone names no transaction, so nothing about
    // one transaction ending makes it unsatisfiable.
    let Discriminator::Response { status: want, cseq_method: Some(method) } = &step.discriminator
    else {
        return false;
    };
    if Method::from_wire(method) != Method::from_wire(&inbound.cseq_method) {
        return false;
    }
    sent == Some(inbound.cseq) && *want != status
}

/// The CSeq of the last request with `method` this leg SENT, where it sent one.
///
/// A leg waiting for a response is the client of that transaction, so its own
/// emission is what identifies the transaction the expect behind it waits on —
/// and a re-INVITE's CSeq is how a final for the original is told from a final
/// for the renegotiation.
fn last_sent(leg: &[RecordedMessage], method: &Method) -> Option<u32> {
    let want = method.clone();
    leg.iter()
        .filter(|recorded| recorded.dir == Dir::Out)
        .filter_map(|recorded| match CustomParser::new().parse(recorded.wire()).ok()? {
            SipMessage::Request(request) if *request.method() == want => Some(request.cseq().seq()),
            _ => None,
        })
        .next_back()
}

#[cfg(test)]
mod tests;
