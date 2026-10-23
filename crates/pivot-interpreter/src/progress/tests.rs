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

fn decide<'a>(inbound: &Inbound, armed: &[Armed<'a>], leg: &[RecordedMessage]) -> Unmatched<'a> {
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
    let u =
        unmatched(&arrival(&response(488, "2 INVITE")), &plain(&steps), &leg, true).expect("armed");
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
    let steps = [expects("s9", 481, Some("BYE"), false), expects("s8", 200, Some("PRACK"), false)];
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
    let u =
        unmatched(&arrival(&response(486, "1 INVITE")), &plain(&steps), &leg, true).expect("armed");
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
