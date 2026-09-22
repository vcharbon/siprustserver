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
mod tests {
    use super::*;
    use crate::plan::{CompiledStep, StepKind};
    use crate::program::StepLoc;
    use crate::recording::Recording;
    use pivot_schema::flow::{Anchor, CheckMode, Delay};
    use pivot_schema::msg::MsgSpec;

    const INVITE: &str = "INVITE sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>\r\n\
        Call-ID: call-a\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    /// A leg's recorded ladder, from `(dir, raw)` pairs in wire order, no step
    /// claiming any of them.
    fn ladder(entries: &[(Dir, String)]) -> Vec<RecordedMessage> {
        let claimed: Vec<(Dir, String, Option<&str>)> =
            entries.iter().map(|(dir, raw)| (*dir, raw.clone(), None)).collect();
        claimed_ladder(&claimed)
    }

    /// A leg's recorded ladder, each datagram under the step that claimed it.
    fn claimed_ladder(entries: &[(Dir, String, Option<&str>)]) -> Vec<RecordedMessage> {
        let recording = Recording::new();
        recording.declare("A");
        for (at, (dir, raw, step)) in entries.iter().enumerate() {
            recording.push("A", *dir, at as u64, raw.clone(), *step, None);
        }
        recording.legs().remove("A").expect("the leg was recorded")
    }

    /// One arriving datagram, projected as the gate projects it.
    fn arrival(raw: &str) -> Inbound {
        let message = CustomParser::new().parse(raw.as_bytes()).expect("parses");
        Inbound::of(&message)
    }

    fn response(status: u16, cseq: &str) -> String {
        format!(
            "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    fn request(method: &str, cseq: &str) -> String {
        format!(
            "{method} sip:alice@127.0.0.1:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    }

    /// A request this leg sends, in its own CSeq space.
    fn sent(method: &str, cseq: u32) -> String {
        INVITE
            .replace("INVITE sip", &format!("{method} sip"))
            .replace("CSeq: 1 INVITE", &format!("CSeq: {cseq} {method}"))
    }

    /// An expect on leg A, armed on `discriminator`, at `order` along the leg.
    fn armed(id: &str, discriminator: Discriminator, optional: bool, order: usize) -> CompiledStep {
        CompiledStep {
            id: id.into(),
            leg: "A".into(),
            kind: StepKind::Expect { check: CheckMode::Record, optional },
            auto: false,
            after: Vec::new(),
            early: None,
            overlap: None,
            discriminator,
            msg: MsgSpec::default(),
            checks: Vec::new(),
            delay: Delay { ms: 0, from: Anchor::Trigger, compressible: false, timer_linked: false },
            within_ms: 1_000,
            retransmits: None,
            retransmit_intervals_ms: Vec::new(),
            loc: StepLoc { item: 0, branch: None, within: 0 },
            order,
            deviations: Vec::new(),
        }
    }

    /// An expect gated on a response.
    fn expects(id: &str, status: u16, cseq_method: Option<&str>, optional: bool) -> CompiledStep {
        let discriminator =
            Discriminator::Response { status, cseq_method: cseq_method.map(str::to_string) };
        let order = id.trim_start_matches('s').parse().unwrap_or(0);
        armed(id, discriminator, optional, order)
    }

    /// An expect gated on a request method.
    fn expects_request(id: &str, method: &str) -> CompiledStep {
        let order = id.trim_start_matches('s').parse().unwrap_or(0);
        armed(id, Discriminator::Request { method: method.into() }, false, order)
    }

    /// The armed set of a leg where every expect is a message item's own and
    /// none has a recorded opener: the transaction is read off the last send.
    fn plain(steps: &[CompiledStep]) -> Vec<Armed<'_>> {
        steps.iter().map(|step| Armed { step, opener: None, retirable: true }).collect()
    }

    /// The armed set of one block: charged, never retired.
    fn block(steps: &[CompiledStep]) -> Vec<Armed<'_>> {
        steps.iter().map(|step| Armed { step, opener: None, retirable: false }).collect()
    }

    /// The armed set with each expect's opening send named.
    fn opened<'a>(steps: &'a [CompiledStep], openers: &[Option<&'a str>]) -> Vec<Armed<'a>> {
        steps
            .iter()
            .zip(openers)
            .map(|(step, opener)| Armed { step, opener: *opener, retirable: true })
            .collect()
    }

    fn decide<'a>(
        inbound: &Inbound,
        armed: &[Armed<'a>],
        leg: &[RecordedMessage],
    ) -> Unmatched<'a> {
        unmatched(inbound, armed, leg, false).expect("something is armed")
    }

    fn ids(steps: &[String]) -> Vec<&str> {
        steps.iter().map(String::as_str).collect()
    }

    /// Two transactions armed together (§6.7c): the INVITE's 600 and the
    /// PRACK's 481. A 200 to the PRACK is charged to the PRACK's expect — the
    /// transaction it rides — whatever stands first on the leg; that expect is
    /// retired, the INVITE's is untouched, and the run goes on.
    #[test]
    fn a_final_is_charged_to_the_armed_expect_of_its_own_transaction() {
        let leg = claimed_ladder(&[
            (Dir::Out, INVITE.to_string(), Some("s1")),
            (Dir::In, response(180, "1 INVITE"), Some("s6")),
            (Dir::Out, sent("PRACK", 2), Some("s7")),
        ]);
        let steps =
            [expects("s11", 600, Some("INVITE"), false), expects("s12", 481, Some("PRACK"), false)];
        let armed = opened(&steps, &[Some("s1"), Some("s7")]);
        let ok = arrival(&response(200, "2 PRACK"));
        let u = decide(&ok, &armed, &leg);
        assert_eq!(u.charged.id, "s12");
        assert!(u.own_transaction);
        assert_eq!(ids(&u.retired), ["s12"]);
        assert!(u.released.is_empty());
        assert!(!u.ends, "a PRACK's final leaves the dialog where it was");
    }

    /// The initial INVITE's final of the OTHER class ends the script: the tail
    /// was scripted for a dialog that now does or does not exist.
    #[test]
    fn an_initial_invite_final_of_the_other_class_retires_and_ends_the_script() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 486, Some("INVITE"), false)];
        let u = decide(&arrival(&response(200, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s7", true));
        assert_eq!(ids(&u.retired), ["s7"]);
        assert!(u.ends, "a 2xx where a reject was named: the tail cannot run");

        let steps = [expects("s7", 200, Some("INVITE"), false)];
        let u = decide(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!(ids(&u.retired), ["s7"]);
        assert!(u.ends, "a reject where the 2xx was named: the tail cannot run");
    }

    /// A re-INVITE's final of the other class leaves the CONFIRMED dialog as it
    /// was: the expect is retired and the tail (ACK, then whatever the document
    /// scripts on the dialog) runs.
    #[test]
    fn a_reinvite_final_of_the_other_class_on_a_confirmed_dialog_goes_on() {
        let leg = ladder(&[
            (Dir::Out, INVITE.to_string()),
            (Dir::In, response(200, "1 INVITE")),
            (Dir::Out, request("ACK", "1 ACK")),
            (Dir::Out, sent("INVITE", 2)),
        ]);
        let steps = [expects("s9", 200, Some("INVITE"), false)];
        let u = unmatched(&arrival(&response(488, "2 INVITE")), &plain(&steps), &leg, true)
            .expect("armed");
        assert_eq!(ids(&u.retired), ["s9"]);
        assert!(!u.ends);
    }

    /// A final of the SAME class as the one named retires the expect and the
    /// run goes on: the ACK behind it composes off whichever final came.
    #[test]
    fn a_final_of_the_same_class_retires_and_goes_on() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 486, Some("INVITE"), false)];
        let u = decide(&arrival(&response(603, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!(ids(&u.retired), ["s7"]);
        assert!(!u.ends);
    }

    /// A final where the step named a PROVISIONAL retires it and ends the
    /// script: the tail was scripted for a transaction still open.
    #[test]
    fn a_final_where_a_provisional_was_named_retires_and_ends_the_script() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s5", 183, Some("INVITE"), false)];
        let u = decide(&arrival(&response(480, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!(ids(&u.retired), ["s5"]);
        assert!(u.ends);
    }

    /// Tolerated provisionals armed ahead of the required final on one
    /// transaction: the charge lands on the REQUIRED step, the optionals are
    /// released with the transaction, and the class rule decides the end.
    #[test]
    fn optionals_ahead_of_the_required_expect_are_released_and_the_required_is_charged() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [
            expects("s5", 183, Some("INVITE"), true),
            expects("s6", 180, Some("INVITE"), true),
            expects("s7", 200, Some("INVITE"), false),
        ];
        let u = decide(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!(u.charged.id, "s7");
        assert_eq!(ids(&u.released), ["s5", "s6"]);
        assert_eq!(ids(&u.retired), ["s7"]);
        assert!(u.ends);
    }

    /// An `optional` alone on the transaction is charged and RELEASED, never
    /// retired, and never ends the script.
    #[test]
    fn an_optional_alone_on_the_transaction_is_charged_and_released() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s5", 180, Some("INVITE"), true)];
        let u = decide(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s5", true));
        assert_eq!(ids(&u.released), ["s5"]);
        assert!(u.retired.is_empty());
        assert!(!u.ends);
    }

    /// No armed expect waits on the datagram's transaction: the charge falls to
    /// the closest step — a discriminator match, else the first armed — and
    /// nothing is retired.
    #[test]
    fn a_final_on_a_transaction_nothing_armed_waits_on_falls_to_the_closest_step() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string()), (Dir::Out, sent("BYE", 3))]);
        let steps =
            [expects("s9", 481, Some("BYE"), false), expects("s8", 200, Some("PRACK"), false)];
        let armed = opened(&steps, &[Some("s3"), None]);
        let u = decide(&arrival(&response(200, "2 PRACK")), &armed, &leg);
        assert_eq!(u.charged.id, "s8", "the discriminator match, wherever it stands");
        assert!(!u.own_transaction);
        assert!(u.retired.is_empty() && u.released.is_empty());
        assert!(!u.ends);

        let steps = [expects("s9", 481, Some("BYE"), false)];
        let u = decide(&arrival(&response(200, "2 PRACK")), &opened(&steps, &[Some("s3")]), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s9", false));
        assert!(u.retired.is_empty());
        assert!(!u.ends);
    }

    /// A block's members are charged — the first in leg order — but never
    /// retired; the block keeps its all-or-none rule.
    #[test]
    fn a_block_member_is_charged_but_never_retired() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let both =
            [expects("s7", 200, Some("INVITE"), false), expects("s8", 486, Some("INVITE"), false)];
        let u = decide(&arrival(&response(603, "1 INVITE")), &block(&both), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s7", true));
        assert!(u.retired.is_empty());
        assert!(u.ends, "neither branch can be satisfied now");
        let u = decide(&arrival(&response(486, "1 INVITE")), &block(&both), &leg);
        assert_eq!(u.charged.id, "s8", "the branch naming the very status");
        assert!(!u.ends);

        let mixed = [expects("s7", 200, Some("INVITE"), false), expects_request("s8", "BYE")];
        let u = decide(&arrival(&response(486, "1 INVITE")), &block(&mixed), &leg);
        assert_eq!(u.charged.id, "s7");
        assert!(u.retired.is_empty());
        assert!(!u.ends, "the BYE branch can still be satisfied");
    }

    /// The transaction is the OPENING SEND's, not the leg's last send of the
    /// method: a tolerated answer overtaken by a second PRACK still takes the
    /// first PRACK's final. Without a recorded opener the last send decides.
    #[test]
    fn the_transaction_is_the_opening_sends_where_one_is_recorded() {
        let leg = claimed_ladder(&[
            (Dir::Out, INVITE.to_string(), Some("s1")),
            (Dir::Out, sent("PRACK", 2), Some("s2")),
            (Dir::Out, sent("PRACK", 3), Some("s4")),
        ]);
        let steps = [expects("s3", 200, Some("PRACK"), true)];
        let u = decide(&arrival(&response(481, "2 PRACK")), &opened(&steps, &[Some("s2")]), &leg);
        assert!(u.own_transaction, "the first PRACK's answer, under its opener");
        assert_eq!(ids(&u.released), ["s3"]);

        let u = decide(&arrival(&response(481, "2 PRACK")), &plain(&steps), &leg);
        assert!(!u.own_transaction, "no opener: the last PRACK sent is CSeq 3");
        assert!(u.released.is_empty());
    }

    /// The SAME status contradicts nothing: what refused it was content or an
    /// early dialog, and a fork's own 2xx or a retransmission can still satisfy
    /// the step. Charged there, retired nowhere, the run goes on.
    #[test]
    fn a_final_of_the_status_the_step_waits_for_retires_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 200, Some("INVITE"), false)];
        let u = decide(&arrival(&response(200, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s7", true));
        assert!(u.retired.is_empty());
        assert!(!u.ends);
    }

    /// A PROVISIONAL ends no transaction: nothing is retired and the run goes
    /// on, the 200 still coming.
    #[test]
    fn a_provisional_that_no_expect_matched_retires_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 200, Some("INVITE"), false)];
        let u = decide(&arrival(&response(183, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!(u.charged.id, "s7");
        assert!(u.retired.is_empty());
        assert!(!u.ends);
    }

    /// A REQUEST nothing matched ends no transaction either — the platform's own
    /// ACK to a 2xx is the case this program meets most.
    #[test]
    fn an_arriving_request_never_retires_nor_blocks() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let ack = arrival(&request("ACK", "1 ACK"));
        let bye = [expects_request("s10", "BYE")];
        let u = decide(&ack, &plain(&bye), &leg);
        assert_eq!(u.charged.id, "s10");
        assert!(!u.ends);
        let ok = [expects("s7", 200, Some("INVITE"), false)];
        let u = decide(&ack, &plain(&ok), &leg);
        assert!(u.retired.is_empty() && !u.ends);
    }

    /// A final for ANOTHER transaction of the same method — a late copy of the
    /// original INVITE's while the re-INVITE's answer is armed — is nobody's:
    /// closest fallback, nothing retired, the run goes on.
    #[test]
    fn a_final_for_another_transaction_of_the_same_method_retires_nothing() {
        let leg = ladder(&[
            (Dir::Out, INVITE.to_string()),
            (Dir::In, response(200, "1 INVITE")),
            (Dir::Out, request("ACK", "1 ACK")),
            (Dir::Out, sent("INVITE", 2)),
        ]);
        let steps = [expects("s9", 200, Some("INVITE"), false)];
        let u = unmatched(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg, true)
            .expect("armed");
        assert!(!u.own_transaction);
        assert!(u.retired.is_empty() && !u.ends);
    }

    /// A final for a transaction this leg never opened is nobody's: there is no
    /// expect of ours behind it.
    #[test]
    fn a_final_for_a_request_this_leg_never_sent_retires_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 481, Some("BYE"), false)];
        let u = decide(&arrival(&response(200, "2 BYE")), &plain(&steps), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s7", false));
        assert!(u.retired.is_empty() && !u.ends);
    }

    /// An expect gated on a status ALONE names no transaction, so no transaction
    /// ending can make it unsatisfiable.
    #[test]
    fn an_expect_naming_no_cseq_method_is_never_retired() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        let steps = [expects("s7", 200, None, false)];
        let u = decide(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg);
        assert_eq!((u.charged.id.as_str(), u.own_transaction), ("s7", false));
        assert!(u.retired.is_empty() && !u.ends);
    }

    /// No armed expect at all is a datagram nothing was waiting for: nothing to
    /// charge here, and the caller states the failure as such.
    #[test]
    fn an_arrival_with_nothing_armed_decides_nothing() {
        let leg = ladder(&[(Dir::Out, INVITE.to_string())]);
        assert!(unmatched(&arrival(&response(486, "1 INVITE")), &[], &leg, false).is_none());
    }
}
