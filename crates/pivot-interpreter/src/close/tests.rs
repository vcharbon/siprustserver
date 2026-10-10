use super::*;

const INVITE: &str = "INVITE sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
        From: <sip:alice@example.test>;tag=a1\r\n\
        To: <sip:bob@example.test>\r\n\
        Call-ID: call-a\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

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

/// The response this leg sends to a [`request`] the peer sent it: the
/// request's own From and To (RFC 3261 §8.2.6.2).
fn reply(status: u16, cseq: &str) -> String {
    format!(
        "SIP/2.0 {status} Whatever\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

/// A request this leg TOOK, on the dialog `to_tag` names, with `extra`
/// headers ahead of the CSeq.
fn taken(method: &str, cseq: &str, to_tag: Option<&str>, extra: &str) -> String {
    let tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    format!(
        "{method} sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{cseq}\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>{tag}\r\n\
             Call-ID: call-a\r\n\
             {extra}CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

/// A reliable provisional this leg SENT under `to_tag` (RFC 3262 §7.1).
fn rang(status: u16, to_tag: &str, rseq: u32) -> String {
    response(status, "1 INVITE")
        .replace("tag=b1", &format!("tag={to_tag}"))
        .replace("CSeq:", &format!("Require: 100rel\r\nRSeq: {rseq}\r\nCSeq:"))
}

/// The answer a taken request is owed, as the close states it.
fn answer(cseq_method: &str, cseq: u32, to_tag: Option<&str>, status: u16) -> Owed {
    Owed::Answer {
        cseq_method: cseq_method.into(),
        cseq,
        to_tag: to_tag.map(str::to_string),
        status,
        early_tag: None,
    }
}

/// [`answer`] riding the early dialog `early_tag` names.
fn answer_under(
    cseq_method: &str,
    cseq: u32,
    to_tag: Option<&str>,
    status: u16,
    early_tag: &str,
) -> Owed {
    Owed::Answer {
        cseq_method: cseq_method.into(),
        cseq,
        to_tag: to_tag.map(str::to_string),
        status,
        early_tag: Some(early_tag.to_string()),
    }
}

/// A ladder's entries: `(dir, raw)`, and whether the datagram is a byte-
/// identical repeat of an earlier one the recording marks as such.
fn record(entries: &[(Dir, String, bool)]) -> Recording {
    let recording = Recording::new();
    recording.declare("A");
    for (at, (dir, raw, repeat)) in entries.iter().enumerate() {
        if *repeat {
            recording.push_repeat("A", *dir, at as u64, raw.clone(), None, None);
        } else {
            recording.push("A", *dir, at as u64, raw.clone(), None, None);
        }
    }
    recording
}

/// A leg's ladder, as `(dir, raw)` pairs in wire order.
fn leg(entries: &[(Dir, String)]) -> Owed {
    let entries: Vec<_> = entries.iter().map(|(d, r)| (*d, r.clone(), false)).collect();
    obligations(&record(&entries)).remove("A").expect("the leg was recorded")
}

/// The same ladder, as the recorded messages [`unscripted`] reads.
fn ladder(entries: &[(Dir, String)]) -> Vec<RecordedMessage> {
    let entries: Vec<_> = entries.iter().map(|(d, r)| (*d, r.clone(), false)).collect();
    record(&entries).legs().remove("A").expect("the leg was recorded")
}

fn message(raw: &str) -> SipMessage {
    parse(raw.as_bytes()).expect("the fixture parses")
}

/// A BYE the leg refused 481 as naming no dialog it holds (RFC 3261
/// §12.2.2) ended nothing: the dialog the leg opened still owes its BYE.
#[test]
fn a_bye_refused_as_naming_no_dialog_ends_nothing() {
    let recording = Recording::new();
    recording.declare("A");
    let foreign_bye = request("BYE", "2 BYE").replace("tag=b1", "tag=b9");
    let refusal = reply(481, "2 BYE").replace("tag=b1", "tag=b9");
    recording.push("A", Dir::Out, 0, INVITE.to_string(), None, None);
    recording.push("A", Dir::In, 1, response(200, "1 INVITE"), None, None);
    recording.push("A", Dir::Out, 2, request("ACK", "1 ACK"), None, None);
    recording.push("A", Dir::In, 3, foreign_bye, None, Some(FOREIGN_DIALOG));
    recording.push("A", Dir::Out, 4, refusal, None, Some(FOREIGN_DIALOG_481));
    assert_eq!(obligations(&recording).remove("A"), Some(Owed::Bye));
}

#[test]
fn a_leg_that_never_spoke_owes_nothing() {
    assert_eq!(leg(&[]), Owed::Nothing);
}

/// The caller's shape, step by step: an INVITE with nothing behind it may
/// not be cancelled (§9.1), one with a provisional may, one answered 2xx is
/// acknowledged and then closed, and a closed one is over.
#[test]
fn the_leg_that_opened_the_dialog_acks_then_byes_and_cancels_before_a_final() {
    let invite = (Dir::Out, INVITE.to_string());
    let alone = leg(std::slice::from_ref(&invite));
    assert_eq!(alone, Owed::AwaitFinal, "§9.1 cancels nothing that has drawn no response");

    let trying = (Dir::In, response(100, "1 INVITE"));
    assert_eq!(leg(&[invite.clone(), trying.clone()]), Owed::Cancel);

    let ok = (Dir::In, response(200, "1 INVITE"));
    let Owed::Ack(acked) = leg(&[invite.clone(), trying.clone(), ok.clone()]) else {
        panic!("a 2xx nothing acknowledged is an ACK owed")
    };
    assert_eq!(acked.status(), 200);

    let ack = (Dir::Out, request("ACK", "1 ACK"));
    assert_eq!(leg(&[invite.clone(), trying.clone(), ok.clone(), ack.clone()]), Owed::Bye);

    let bye = (Dir::Out, request("BYE", "2 BYE"));
    assert_eq!(leg(&[invite, trying, ok, ack, bye]), Owed::Nothing, "the dialog is over");
}

/// A re-INVITE in flight when the script ends closes in ORDER: the
/// re-INVITE's own 2xx is acknowledged first — per its own CSeq, not the
/// original's — and the BYE that ends the dialog follows.
#[test]
fn a_reinvited_dialog_acks_the_reinvites_final_before_its_bye() {
    let mut ladder = vec![
        (Dir::Out, INVITE.to_string()),
        (Dir::In, response(200, "1 INVITE")),
        (Dir::Out, request("ACK", "1 ACK")),
        (Dir::Out, INVITE.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE")),
        (Dir::In, response(200, "2 INVITE")),
    ];
    let Owed::Ack(acked) = leg(&ladder) else {
        panic!("the re-INVITE's unacknowledged 2xx is an ACK owed")
    };
    assert_eq!(acked.cseq().seq(), 2, "the re-INVITE's final, not the original's");

    ladder.push((Dir::Out, request("ACK", "2 ACK")));
    assert_eq!(leg(&ladder), Owed::Bye, "acknowledged, the dialog still owes its BYE");
}

/// A non-2xx final is acknowledged too (§17.1.1.3), and closes nothing.
#[test]
fn a_refused_invite_is_acknowledged_and_then_holds_nothing_open() {
    let invite = (Dir::Out, INVITE.to_string());
    let busy = (Dir::In, response(486, "1 INVITE"));
    assert!(matches!(leg(&[invite.clone(), busy.clone()]), Owed::Ack(_)));
    let ack = (Dir::Out, request("ACK", "1 ACK"));
    assert_eq!(leg(&[invite, busy, ack]), Owed::Nothing, "no dialog to BYE");
}

/// A CANCEL already sent leaves nothing to do but take the final it draws —
/// and then acknowledge it (§17.1.1.3). The leg is not DONE meanwhile: a
/// close that walked away here would leave the INVITE transaction to
/// retransmit its reject to Timer H.
#[test]
fn a_cancel_already_sent_waits_for_its_final_and_acknowledges_it() {
    let invite = (Dir::Out, INVITE.to_string());
    let ringing = (Dir::In, response(180, "1 INVITE"));
    let cancel = (Dir::Out, request("CANCEL", "1 CANCEL"));
    let owed = leg(&[invite.clone(), ringing.clone(), cancel.clone()]);
    assert_eq!(owed, Owed::AwaitFinal);
    assert!(owed.open(), "the 487 is still coming, and still owes an ACK");
    let terminated = (Dir::In, response(487, "1 INVITE"));
    assert!(matches!(leg(&[invite, ringing, cancel, terminated]), Owed::Ack(_)));
}

/// The callee's shape: the side that ANSWERED never starts the teardown, and
/// the BYE that arrives is answered rather than crossed.
#[test]
fn the_leg_that_answered_the_dialog_waits_for_the_teardown_and_answers_it() {
    let invite = (Dir::In, INVITE.to_string());
    // An INVITE it has not answered is answered now: the script stopped, and
    // the platform is holding a transaction open on it.
    assert_eq!(leg(std::slice::from_ref(&invite)), answer("INVITE", 1, None, 480));

    let ok = (Dir::Out, response(200, "1 INVITE"));
    assert_eq!(leg(&[invite.clone(), ok.clone()]), Owed::AwaitTeardown);

    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
    assert_eq!(leg(&[invite.clone(), ok.clone(), ack.clone()]), Owed::AwaitTeardown);

    let bye = (Dir::In, taken("BYE", "2 BYE", Some("b1"), ""));
    assert_eq!(
        leg(&[invite.clone(), ok.clone(), ack.clone(), bye.clone()]),
        answer("BYE", 2, Some("b1"), 200)
    );

    let answered = (Dir::Out, response(200, "2 BYE"));
    assert_eq!(leg(&[invite, ok, ack, bye, answered]), Owed::Nothing);
}

/// A CANCEL jumps the queue and takes the INVITE with it: 200 to the CANCEL
/// first (§9.2), then the 487 the cancelled INVITE ends with.
#[test]
fn a_cancelled_invite_is_answered_487_and_the_cancel_itself_first() {
    let invite = (Dir::In, INVITE.to_string());
    let ringing = (Dir::Out, response(180, "1 INVITE"));
    let cancel = (Dir::In, taken("CANCEL", "1 CANCEL", None, ""));
    assert_eq!(
        leg(&[invite.clone(), ringing.clone(), cancel.clone()]),
        answer("CANCEL", 1, None, 200)
    );
    let answered = (Dir::Out, response(200, "1 CANCEL"));
    assert_eq!(leg(&[invite, ringing, cancel, answered]), answer("INVITE", 1, None, 487));
}

/// RFC 3262 §3, both halves: a PRACK whose RAck names the reliable
/// provisional this leg sent and nothing has acknowledged draws a 200, and
/// one naming anything else — another RSeq, a provisional a PRACK it
/// answered already acknowledged, or no well-formed RAck at all — draws a
/// 481. Once answered, the same PRACK owes nothing more.
#[test]
fn an_unscripted_prack_draws_200_for_the_provisional_it_names_and_481_otherwise() {
    let invite = (Dir::In, INVITE.to_string());
    let reliable = (Dir::Out, rang(180, "b1", 7));
    let prack_raw = taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
    let prack = (Dir::In, prack_raw.clone());
    let ok = answer("PRACK", 2, Some("b1"), 200);

    let named = ladder(&[invite.clone(), reliable.clone(), prack.clone()]);
    assert_eq!(unscripted(&named, &message(&prack_raw)), Some(ok.clone()));
    // The generic close answers it ahead of the older INVITE transaction:
    // the PRACK's client transaction is the one running a timer, and a
    // non-2xx final first would end the early dialog it rides (§12.3).
    assert_eq!(leg(&[invite.clone(), reliable.clone(), prack.clone()]), ok);

    let other_raw = prack_raw.replace("RAck: 7 1 INVITE", "RAck: 9 1 INVITE");
    let other = ladder(&[invite.clone(), reliable.clone(), (Dir::In, other_raw.clone())]);
    let refused = Some(answer("PRACK", 2, Some("b1"), 481));
    assert_eq!(unscripted(&other, &message(&other_raw)), refused);

    let bare_raw = taken("PRACK", "2 PRACK", Some("b1"), "");
    let bare = ladder(&[invite.clone(), reliable.clone(), (Dir::In, bare_raw.clone())]);
    assert_eq!(unscripted(&bare, &message(&bare_raw)), refused, "no RAck names nothing");

    let answered = (Dir::Out, response(200, "2 PRACK"));
    let done = ladder(&[invite.clone(), reliable.clone(), prack.clone(), answered.clone()]);
    assert_eq!(unscripted(&done, &message(&prack_raw)), None, "one final per transaction");
    let again_raw = taken("PRACK", "3 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
    let again = ladder(&[invite, reliable, prack, answered, (Dir::In, again_raw.clone())]);
    assert_eq!(unscripted(&again, &message(&again_raw)), Some(answer("PRACK", 3, Some("b1"), 481)));
}

/// The CANCEL pair is the CANCEL's own whatever else the leg holds open: a
/// PRACK still unanswered does not stand between the 200 to the CANCEL and
/// the 487 to the INVITE it names (§9.2).
#[test]
fn an_unscripted_cancel_still_draws_the_487_past_an_open_prack() {
    let cancel_raw = taken("CANCEL", "1 CANCEL", None, "");
    let ladder = ladder(&[
        (Dir::In, INVITE.to_string()),
        (Dir::Out, rang(180, "b1", 7)),
        (Dir::In, taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n")),
        (Dir::In, cancel_raw.clone()),
        (Dir::Out, response(200, "1 CANCEL")),
    ]);
    assert_eq!(unscripted(&ladder, &message(&cancel_raw)), Some(answer("INVITE", 1, None, 487)));
}

/// A byte-identical repeat of a request the leg already answered re-opens
/// nothing: the server transaction is Completed and RFC 3261 §17.2.2 lets it
/// re-send the last response or stay silent, never emit a second final.
#[test]
fn a_repeat_of_an_answered_request_reopens_no_transaction() {
    let prack = taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n");
    let recording = record(&[
        (Dir::In, INVITE.to_string(), false),
        (Dir::Out, rang(180, "b1", 7), false),
        (Dir::In, prack.clone(), false),
        (Dir::Out, response(200, "2 PRACK"), false),
        // The peer's retransmission crossed the 200 on the wire.
        (Dir::In, prack, true),
        (Dir::Out, response(486, "1 INVITE"), false),
    ]);
    assert_eq!(obligations(&recording).remove("A"), Some(Owed::Nothing));
}

/// RFC 3262 §3 matches a PRACK within the SAME early dialog as the
/// provisional: two forks on one leg may each ring under `RSeq: 1` and be
/// acknowledged by a PRACK numbered 2 in their own space, so the dialog's
/// To-tag is part of the key — for the provisional, the PRACK, and the
/// answer that closes the PRACK's transaction.
#[test]
fn a_prack_is_matched_within_its_own_early_dialog() {
    let invite = (Dir::In, INVITE.to_string());
    let fork1 = (Dir::Out, rang(183, "t1", 1));
    let fork2 = (Dir::Out, rang(180, "t2", 1));
    let prack1_raw = taken("PRACK", "2 PRACK", Some("t1"), "RAck: 1 1 INVITE\r\n");
    let prack2_raw = taken("PRACK", "2 PRACK", Some("t2"), "RAck: 1 1 INVITE\r\n");
    let prack1 = (Dir::In, prack1_raw.clone());
    let prack2 = (Dir::In, prack2_raw.clone());
    let ok1 = (Dir::Out, response(200, "2 PRACK").replace("tag=b1", "tag=t1"));

    // Fork 1's 200 closes fork 1's PRACK only: fork 2's is still owed its own.
    let both = ladder(&[invite.clone(), fork1.clone(), fork2.clone(), prack1, prack2, ok1]);
    assert_eq!(unscripted(&both, &message(&prack2_raw)), Some(answer("PRACK", 2, Some("t2"), 200)));
    // A PRACK carrying fork 1's RAck under fork 2's tag names no provisional
    // of fork 2's.
    let crossed_raw = prack1_raw.replace("tag=t1", "tag=t2").replace("RAck: 1", "RAck: 3");
    let crossed = ladder(&[invite, fork1, fork2, (Dir::In, crossed_raw.clone())]);
    assert_eq!(
        unscripted(&crossed, &message(&crossed_raw)),
        Some(answer("PRACK", 2, Some("t2"), 481))
    );
}

/// Two PRACKs open on one dialog are answered oldest first, and each with
/// ITS OWN status: the obligation names the transaction it answers, not
/// only the method.
#[test]
fn two_open_pracks_are_answered_in_order_each_with_its_own_status() {
    let invite = (Dir::In, INVITE.to_string());
    let reliable = (Dir::Out, rang(180, "b1", 7));
    let named = (Dir::In, taken("PRACK", "2 PRACK", Some("b1"), "RAck: 7 1 INVITE\r\n"));
    let stray = (Dir::In, taken("PRACK", "3 PRACK", Some("b1"), "RAck: 9 1 INVITE\r\n"));
    let mut ladder = vec![invite, reliable, named, stray];
    assert_eq!(leg(&ladder), answer("PRACK", 2, Some("b1"), 200));
    ladder.push((Dir::Out, response(200, "2 PRACK")));
    assert_eq!(leg(&ladder), answer("PRACK", 3, Some("b1"), 481));
}

/// A method the RFC gives no termination answer for is left alone: it holds
/// no call up, and answering it would put a message on the wire nothing
/// asked for.
#[test]
fn a_request_no_rule_answers_is_left_to_the_platform() {
    let options = (Dir::In, taken("OPTIONS", "9 OPTIONS", None, ""));
    assert_eq!(leg(&[options]), Owed::Nothing);
}

/// The act each obligation puts on the wire, and the ones that put none.
#[test]
fn only_an_obligation_with_an_emission_names_an_act() {
    assert_eq!(Owed::Nothing.act(), None);
    assert_eq!(Owed::AwaitFinal.act(), None);
    assert_eq!(Owed::AwaitTeardown.act(), None);
    assert_eq!(Owed::Bye.act(), Some(CloseOwed::Bye));
    assert_eq!(Owed::Cancel.act(), Some(CloseOwed::Cancel));
    assert_eq!(answer("BYE", 2, Some("b1"), 200).act(), Some(CloseOwed::Answer));
    // Waiting is still OPEN: the close is not finished while a teardown the
    // far side owes has not arrived.
    assert!(Owed::AwaitTeardown.open());
    assert!(!Owed::Nothing.open());
}

/// §9.2, both halves: the CANCEL is answered 200, and once that is on the
/// recording the INVITE it named is answered 487.
#[test]
fn an_unscripted_cancel_draws_the_200_and_then_the_487() {
    let invite = (Dir::In, INVITE.to_string());
    let ringing = (Dir::Out, response(180, "1 INVITE"));
    let raw = taken("CANCEL", "1 CANCEL", None, "");
    let cancel = (Dir::In, raw.clone());
    let trigger = message(&raw);

    let first = ladder(&[invite.clone(), ringing.clone(), cancel.clone()]);
    assert_eq!(unscripted(&first, &trigger), Some(answer("CANCEL", 1, None, 200)));

    let answered = (Dir::Out, response(200, "1 CANCEL"));
    let second = ladder(&[invite.clone(), ringing.clone(), cancel.clone(), answered.clone()]);
    assert_eq!(unscripted(&second, &trigger), Some(answer("INVITE", 1, None, 487)));

    let terminated = (Dir::Out, response(487, "1 INVITE"));
    let third = ladder(&[invite, ringing, cancel, answered, terminated]);
    assert_eq!(unscripted(&third, &trigger), None, "the pair is discharged");
}

/// §17.1.1.3: a non-2xx final this leg took for the INVITE it sent is ACKed,
/// and the ACK names that final's own transaction.
#[test]
fn an_unscripted_non_2xx_final_draws_an_ack_for_its_own_transaction() {
    let invite = (Dir::Out, INVITE.to_string());
    let raw = response(487, "1 INVITE");
    let terminated = (Dir::In, raw.clone());
    let trigger = message(&raw);

    let taken = ladder(&[invite.clone(), terminated.clone()]);
    let Some(Owed::Ack(acked)) = unscripted(&taken, &trigger) else {
        panic!("a non-2xx final nothing acknowledged is an ACK owed")
    };
    assert_eq!((acked.status(), acked.cseq().seq()), (487, 1));

    let ack = (Dir::Out, request("ACK", "1 ACK"));
    assert_eq!(
        unscripted(&ladder(&[invite, terminated, ack]), &trigger),
        None,
        "the obligation is discharged once the ACK is on the wire"
    );
}

/// A 2xx is NOT this seam's: RFC 3261 §13.2.2.4 leaves it to the dialog the
/// flow itself scripts an ACK for.
#[test]
fn an_unscripted_2xx_final_draws_nothing_here() {
    let invite = (Dir::Out, INVITE.to_string());
    let raw = response(200, "1 INVITE");
    let ok = (Dir::In, raw.clone());
    assert_eq!(unscripted(&ladder(&[invite, ok]), &message(&raw)), None);
}

/// Every other unscripted arrival is refused and answered by nobody: no
/// rule makes an INFO or an OPTIONS the transaction layer's own.
#[test]
fn an_unscripted_arrival_of_any_other_method_draws_no_invented_answer() {
    let invite = (Dir::In, INVITE.to_string());
    let ok = (Dir::Out, response(200, "1 INVITE"));
    for (method, tag) in [("OPTIONS", None), ("INFO", Some("b1"))] {
        let raw = taken(method, &format!("9 {method}"), tag, "");
        let arrival = (Dir::In, raw.clone());
        assert_eq!(
            unscripted(&ladder(&[invite.clone(), ok.clone(), arrival]), &message(&raw)),
            None,
            "{method} is nobody's transaction obligation"
        );
    }
}

/// RFC 6665 §4.1.3 (RFC 3515 §2.4.4 for the subscription a REFER creates):
/// a subscriber answers every NOTIFY of its subscription, so a NOTIFY on
/// the dialog this leg holds, for the REFER it sent, draws `200` where no
/// step scripts it — the notifier is otherwise left retransmitting to its
/// Timer F, which ends the dialog under it.
#[test]
fn an_unscripted_notify_of_the_leg_s_own_refer_draws_200() {
    let invite = (Dir::In, INVITE.to_string());
    let ok = (Dir::Out, response(200, "1 INVITE"));
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
    let refer = (Dir::Out, request("REFER", "2 REFER"));
    let accepted = (Dir::In, reply(202, "2 REFER"));
    let notify_raw =
        taken("NOTIFY", "2 NOTIFY", Some("b1"), "Event: refer\r\nSubscription-State: active\r\n");
    let notify = (Dir::In, notify_raw.clone());
    let held = ladder(&[invite, ok, ack, refer, accepted, notify]);
    assert_eq!(
        unscripted(&held, &message(&notify_raw)),
        Some(answer("NOTIFY", 2, Some("b1"), 200)),
        "the subscriber owes the NOTIFY its final"
    );
}

/// RFC 3261 §15.1.2: a BYE on the dialog this leg holds is answered `200`,
/// once, from whichever side of the dialog this leg is on — and while this
/// leg's own BYE still awaits its final (§15.1.1). A BYE matching no
/// dialog of this leg's — another tag, no tag, one an answered BYE already
/// ended — draws the `481` the same section states.
#[test]
fn an_unscripted_bye_draws_200_on_the_held_dialog_and_481_on_none() {
    // The side that ANSWERED the dialog holds it under the tag its 2xx minted.
    let invite = (Dir::In, INVITE.to_string());
    let ok = (Dir::Out, response(200, "1 INVITE"));
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
    let bye_raw = taken("BYE", "2 BYE", Some("b1"), "");
    let bye = (Dir::In, bye_raw.clone());
    let held = ladder(&[invite.clone(), ok.clone(), ack.clone(), bye.clone()]);
    assert_eq!(unscripted(&held, &message(&bye_raw)), Some(answer("BYE", 2, Some("b1"), 200)));
    let answered = (Dir::Out, response(200, "2 BYE"));
    let done = ladder(&[invite.clone(), ok.clone(), ack.clone(), bye, answered.clone()]);
    assert_eq!(unscripted(&done, &message(&bye_raw)), None, "one final per transaction");

    let other_raw = taken("BYE", "2 BYE", Some("b2"), "");
    let other = ladder(&[invite.clone(), ok.clone(), ack.clone(), (Dir::In, other_raw.clone())]);
    assert_eq!(
        unscripted(&other, &message(&other_raw)),
        Some(answer("BYE", 2, Some("b2"), 481)),
        "another tag is no dialog of ours"
    );

    let bare_raw = taken("BYE", "2 BYE", None, "");
    let bare = ladder(&[invite.clone(), ok.clone(), ack.clone(), (Dir::In, bare_raw.clone())]);
    assert_eq!(
        unscripted(&bare, &message(&bare_raw)),
        Some(answer("BYE", 2, None, 481)),
        "no tag names no dialog"
    );

    let ringing = (Dir::Out, response(180, "1 INVITE"));
    let again_raw = taken("BYE", "3 BYE", Some("b1"), "");
    let again = ladder(&[
        invite.clone(),
        ringing,
        ok.clone(),
        ack.clone(),
        (Dir::In, bye_raw.clone()),
        answered,
        (Dir::In, again_raw.clone()),
    ]);
    assert_eq!(
        unscripted(&again, &message(&again_raw)),
        Some(answer("BYE", 3, Some("b1"), 481)),
        "the dialog it rang and confirmed under is over"
    );

    // A 200 to a BYE on another tag (the generic close answers every
    // BYE) ends only that tag's dialog: the held one is still ours.
    let stray = (Dir::In, taken("BYE", "3 BYE", Some("b9"), ""));
    let stray_ok = (Dir::Out, response(200, "3 BYE").replace("tag=b1", "tag=b9"));
    let still_held = ladder(&[invite, ok, ack, stray, stray_ok, (Dir::In, bye_raw.clone())]);
    assert_eq!(
        unscripted(&still_held, &message(&bye_raw)),
        Some(answer("BYE", 2, Some("b1"), 200)),
        "the held dialog outlives another tag's BYE"
    );

    // The side that OPENED the dialog holds it under its INVITE's From-tag.
    let sent = (Dir::Out, INVITE.to_string());
    let took = (Dir::In, response(200, "1 INVITE"));
    let acked = (Dir::Out, request("ACK", "1 ACK"));
    let peer_raw = request("BYE", "2 BYE");
    let peer = (Dir::In, peer_raw.clone());
    let opened = ladder(&[sent.clone(), took.clone(), acked.clone(), peer.clone()]);
    assert_eq!(unscripted(&opened, &message(&peer_raw)), Some(answer("BYE", 2, Some("a1"), 200)));

    // A BYE crossing ours: the dialog ends on OUR BYE's final (§15.1.1),
    // and until then the peer's names it.
    let ours = (Dir::Out, request("BYE", "2 BYE"));
    let crossed_raw = request("BYE", "3 BYE");
    let crossed = ladder(&[sent, took, acked, ours, (Dir::In, crossed_raw.clone())]);
    assert_eq!(
        unscripted(&crossed, &message(&crossed_raw)),
        Some(answer("BYE", 3, Some("a1"), 200)),
        "our BYE has no final yet"
    );
}

/// RFC 3261 §15: a BYE may end an EARLY dialog — one a tagged provisional
/// opened (§12.1.1) and no final has confirmed or ended — and §15.1.2
/// answers it like any dialog's: `200`, then the `487` the INVITE that
/// dialog rides is still owed, under the tag the dialog rang with
/// (§8.2.6.2). A tag that rang and then took a 2xx is the held dialog;
/// one that rang and then took a non-2xx is gone (§12.3), so its BYE
/// draws `481`. On the side that OPENED the dialog the early dialog is
/// the peer's tagged provisional, and the BYE it sends on it draws the
/// `200` too — the INVITE stays the peer's to answer, so no second act.
#[test]
fn an_unscripted_bye_on_an_early_dialog_draws_200_then_487s_the_invite_it_rides() {
    // The side that ANSWERED: ringing under b1, no final yet.
    let invite = (Dir::In, INVITE.to_string());
    let ringing = (Dir::Out, response(180, "1 INVITE"));
    let bye_raw = taken("BYE", "2 BYE", Some("b1"), "");
    let bye = (Dir::In, bye_raw.clone());
    let trigger = message(&bye_raw);
    let early = ladder(&[invite.clone(), ringing.clone(), bye.clone()]);
    assert_eq!(unscripted(&early, &trigger), Some(answer("BYE", 2, Some("b1"), 200)));

    let answered = (Dir::Out, response(200, "2 BYE"));
    let ended = ladder(&[invite.clone(), ringing.clone(), bye.clone(), answered.clone()]);
    let terminated = answer_under("INVITE", 1, None, 487, "b1");
    assert_eq!(
        unscripted(&ended, &trigger),
        Some(terminated.clone()),
        "the INVITE the early dialog rides, under the tag it rang with"
    );
    // The generic close reads the same fact: the pending INVITE owes the
    // 487 the ended dialog recommends, not the 480 of a script that stopped.
    assert_eq!(leg(&[invite.clone(), ringing.clone(), bye.clone(), answered.clone()]), terminated);

    let again_raw = taken("BYE", "3 BYE", Some("b1"), "");
    let again = ladder(&[
        invite.clone(),
        ringing.clone(),
        bye.clone(),
        answered.clone(),
        (Dir::In, again_raw.clone()),
    ]);
    assert_eq!(
        unscripted(&again, &message(&again_raw)),
        Some(answer("BYE", 3, Some("b1"), 481)),
        "the early dialog that BYE ended is gone"
    );

    let final_out = (Dir::Out, response(487, "1 INVITE"));
    let done = ladder(&[
        invite.clone(),
        ringing.clone(),
        bye.clone(),
        answered.clone(),
        final_out.clone(),
    ]);
    assert_eq!(unscripted(&done, &trigger), None, "the pair is discharged");

    // Two forks ringing: the 487 ends the INVITE transaction, and with it
    // EVERY early dialog it opened (§12.3) — the other fork's BYE is late.
    let second_fork = (Dir::Out, response(183, "1 INVITE").replace("tag=b1", "tag=b2"));
    let other_raw = taken("BYE", "3 BYE", Some("b2"), "");
    let forked = ladder(&[
        invite.clone(),
        ringing.clone(),
        second_fork,
        bye,
        answered,
        final_out,
        (Dir::In, other_raw.clone()),
    ]);
    assert_eq!(
        unscripted(&forked, &message(&other_raw)),
        Some(answer("BYE", 3, Some("b2"), 481)),
        "the non-2xx ended the other fork's early dialog too"
    );

    // Rang and then refused: the early dialog ended with the non-2xx.
    let refused = (Dir::Out, response(486, "1 INVITE"));
    let gone = ladder(&[invite.clone(), ringing.clone(), refused, (Dir::In, bye_raw.clone())]);
    assert_eq!(
        unscripted(&gone, &trigger),
        Some(answer("BYE", 2, Some("b1"), 481)),
        "the dialog it rang under is over"
    );

    // Rang and then confirmed: the held dialog, under the same tag.
    let ok = (Dir::Out, response(200, "1 INVITE"));
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
    let held = ladder(&[invite, ringing, ok, ack, (Dir::In, bye_raw)]);
    assert_eq!(unscripted(&held, &trigger), Some(answer("BYE", 2, Some("b1"), 200)));

    // The side that OPENED: the peer rang under b1, and BYEs that dialog.
    let sent = (Dir::Out, INVITE.to_string());
    let took = (Dir::In, response(180, "1 INVITE"));
    let peer_raw = request("BYE", "2 BYE");
    let peer = (Dir::In, peer_raw.clone());
    let opened = ladder(&[sent.clone(), took.clone(), peer.clone()]);
    assert_eq!(unscripted(&opened, &message(&peer_raw)), Some(answer("BYE", 2, Some("a1"), 200)));
    let peer_answered = (Dir::Out, reply(200, "2 BYE"));
    let peer_ended = ladder(&[sent.clone(), took.clone(), peer.clone(), peer_answered.clone()]);
    assert_eq!(
        unscripted(&peer_ended, &message(&peer_raw)),
        None,
        "the INVITE this leg sent is the peer's to answer"
    );
    let peer_again_raw = request("BYE", "3 BYE");
    let peer_again = ladder(&[
        sent.clone(),
        took.clone(),
        peer,
        peer_answered,
        (Dir::In, peer_again_raw.clone()),
    ]);
    assert_eq!(
        unscripted(&peer_again, &message(&peer_again_raw)),
        Some(answer("BYE", 3, Some("a1"), 481)),
        "the early dialog that BYE ended is gone"
    );

    // A BYE from a tag the peer never rang under names no early dialog.
    let stranger_raw = peer_raw.replace("tag=b1", "tag=b2");
    let stranger = ladder(&[sent.clone(), took.clone(), (Dir::In, stranger_raw.clone())]);
    assert_eq!(
        unscripted(&stranger, &message(&stranger_raw)),
        Some(answer("BYE", 2, Some("a1"), 481)),
        "no early dialog under that tag"
    );

    // The peer's non-2xx ended its early dialog before the BYE.
    let peer_refused = (Dir::In, response(486, "1 INVITE"));
    let acked = (Dir::Out, request("ACK", "1 ACK"));
    let peer_gone = ladder(&[
        sent.clone(),
        took.clone(),
        peer_refused.clone(),
        acked.clone(),
        (Dir::In, peer_raw.clone()),
    ]);
    assert_eq!(
        unscripted(&peer_gone, &message(&peer_raw)),
        Some(answer("BYE", 2, Some("a1"), 481)),
        "the early dialog ended with the 486"
    );
    // The peer rang two forks: the 486 under one ends both early dialogs
    // (§12.3), so the other fork's BYE names no dialog either.
    let took_other = (Dir::In, response(183, "1 INVITE").replace("tag=b1", "tag=b2"));
    let forked_gone =
        ladder(&[sent, took, took_other, peer_refused, acked, (Dir::In, stranger_raw.clone())]);
    assert_eq!(
        unscripted(&forked_gone, &message(&stranger_raw)),
        Some(answer("BYE", 2, Some("a1"), 481)),
        "the non-2xx ended the other fork's early dialog too"
    );
}

/// RFC 3261 §15.1.2, last paragraph: the UAS that answered a BYE still
/// responds to every request pending on that dialog, 487 recommended. A
/// re-INVITE the flow scripted nothing for is open on the leg when the
/// BYE lands: the BYE draws its 200 first, then the INVITE its 487, and a
/// third call owes nothing. A BYE answered 481 ended no dialog, so the
/// INVITE it left open stays the platform's to answer.
#[test]
fn an_unscripted_bye_answered_200_then_487s_the_re_invite_pending_on_that_dialog() {
    let invite = (Dir::In, INVITE.to_string());
    let ok = (Dir::Out, response(200, "1 INVITE"));
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""));
    let reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b1"), ""));
    let bye_raw = taken("BYE", "3 BYE", Some("b1"), "");
    let bye = (Dir::In, bye_raw.clone());
    let trigger = message(&bye_raw);

    let first = ladder(&[invite.clone(), ok.clone(), ack.clone(), reinvite.clone(), bye.clone()]);
    assert_eq!(unscripted(&first, &trigger), Some(answer("BYE", 3, Some("b1"), 200)));

    let answered = (Dir::Out, response(200, "3 BYE"));
    let second = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        answered.clone(),
    ]);
    assert_eq!(
        unscripted(&second, &trigger),
        Some(answer("INVITE", 2, Some("b1"), 487)),
        "the re-INVITE pending on the ended dialog"
    );
    assert_eq!(
        leg(&[
            invite.clone(),
            ok.clone(),
            ack.clone(),
            reinvite.clone(),
            bye.clone(),
            answered.clone()
        ]),
        answer("INVITE", 2, Some("b1"), 487),
        "the generic close reads the ended dialog too"
    );

    let terminated = (Dir::Out, response(487, "2 INVITE"));
    let third = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite,
        bye.clone(),
        answered.clone(),
        terminated,
    ]);
    assert_eq!(unscripted(&third, &trigger), None, "the pair is discharged");

    // A BYE on no dialog of ours ends nothing: its 481 leaves the INVITE alone.
    let stray_raw = taken("BYE", "3 BYE", Some("b2"), "");
    let stray_reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b2"), ""));
    let refused = (Dir::Out, response(481, "3 BYE").replace("tag=b1", "tag=b2"));
    let stray = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        stray_reinvite.clone(),
        (Dir::In, stray_raw.clone()),
        refused,
    ]);
    assert_eq!(unscripted(&stray, &message(&stray_raw)), None, "no dialog of ours ended");

    // The 200 ended OUR dialog; an INVITE pending under another tag is on
    // another dialog and is not this BYE's to answer.
    let other = ladder(&[invite, ok, ack, stray_reinvite, bye, answered]);
    assert_eq!(unscripted(&other, &trigger), None, "a pending INVITE on another dialog");
}

/// A non-2xx final this leg sent to an INVITE holds its server transaction
/// in Completed until the peer's ACK (RFC 3261 §17.2.1, §17.1.1.3): listed
/// from its first emission — a retransmission re-opens no clock — until
/// the ACK naming its CSeq. A 2xx is the peer's own request to acknowledge
/// (§13.2.2.4) and a non-INVITE final is never ACKed: neither is listed.
#[test]
fn a_non_2xx_invite_final_this_leg_sent_awaits_the_peer_s_ack() {
    let invite = (Dir::In, INVITE.to_string(), false);
    let ok = (Dir::Out, response(200, "1 INVITE"), false);
    let ack = (Dir::In, taken("ACK", "1 ACK", Some("b1"), ""), false);
    let reinvite = (Dir::In, taken("INVITE", "2 INVITE", Some("b1"), ""), false);
    let bye = (Dir::In, taken("BYE", "3 BYE", Some("b2"), ""), false);
    let refused = (Dir::Out, response(481, "3 BYE").replace("tag=b1", "tag=b2"), false);
    let terminated = (Dir::Out, response(487, "2 INVITE"), false);
    let again = (Dir::Out, response(487, "2 INVITE"), true);
    let ladder = |entries: &[(Dir, String, bool)]| {
        unacked_finals(&record(entries).legs().remove("A").expect("the leg was recorded"))
    };

    assert_eq!(ladder(&[invite.clone(), ok.clone(), ack.clone()]), [], "a 2xx owes nothing");
    assert_eq!(
        ladder(&[invite.clone(), ok.clone(), ack.clone(), bye.clone(), refused.clone()]),
        [],
        "a non-INVITE final owes nothing"
    );
    let sent = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
    ]);
    assert_eq!(sent, [UnackedFinal { cseq: 2, status: 487, sent_at_us: 6 }]);
    let acked = (Dir::In, taken("ACK", "2 ACK", Some("b1"), ""), false);
    let discharged = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        acked.clone(),
    ]);
    assert_eq!(discharged, [], "the peer's ACK ended the transaction");
    // A second final on the transaction the 487 already ended answers
    // nothing the peer holds open: it neither re-opens the CSeq nor moves
    // the first final's clock, before the ACK or after it.
    let late = (Dir::Out, response(408, "2 INVITE"), false);
    let before_ack = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        late.clone(),
    ]);
    assert_eq!(before_ack, [UnackedFinal { cseq: 2, status: 487, sent_at_us: 6 }]);
    let then_acked = ladder(&[
        invite.clone(),
        ok.clone(),
        ack.clone(),
        reinvite.clone(),
        bye.clone(),
        refused.clone(),
        terminated.clone(),
        again.clone(),
        late.clone(),
        acked.clone(),
    ]);
    assert_eq!(then_acked, [], "the ACK still discharges the 487");
    let after_ack =
        ladder(&[invite, ok, ack, reinvite, bye, refused, terminated, again, acked, late]);
    assert_eq!(after_ack, [], "a final on an ended transaction is owed no ACK");
}

/// A leg holding an unanswered request that is NOT the cancelled INVITE
/// answers nothing for it: the CANCEL's own pair is the whole scope.
#[test]
fn a_cancel_answers_its_own_pair_and_no_other_request_the_leg_holds() {
    let options = (Dir::In, taken("OPTIONS", "9 OPTIONS", None, ""));
    let raw = taken("CANCEL", "1 CANCEL", None, "");
    let cancel = (Dir::In, raw.clone());
    let trigger = message(&raw);
    let answered = (Dir::Out, response(200, "1 CANCEL"));
    assert_eq!(
        unscripted(&ladder(&[options.clone(), cancel.clone()]), &trigger),
        Some(answer("CANCEL", 1, None, 200))
    );
    assert_eq!(
        unscripted(&ladder(&[options, cancel, answered]), &trigger),
        None,
        "the OPTIONS this leg holds is not the CANCEL's to answer"
    );
}
