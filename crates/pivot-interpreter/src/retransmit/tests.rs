use super::*;

/// A run window wide enough that a CLOSED ladder's dwell is what bounds it.
const RUN_END: u64 = 60_000_000;

const fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

const INVITE: &str = "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

fn ladder_of(raw: &str) -> Result<Schedule, String> {
    schedule_of(raw.as_bytes(), &[])
}

/// A captured platform's ladder is not the RFC's, and where the document
/// measured it the document wins (§6.9).
#[test]
fn a_measured_ladder_paces_by_the_documents_own_gaps() {
    let ladder = schedule_of(INVITE.as_bytes(), &[408, 17_480])
        .expect("a document that states its gaps paces by them");
    assert_eq!(ladder.interval(1), Some(ms(408)));
    assert_eq!(ladder.interval(2), Some(ms(17_480)));
    assert_eq!(
        ladder.interval(3),
        Some(ms(17_480)),
        "a count longer than its gaps repeats the last, never falls back onto a class ladder"
    );
}

/// A class is refused for having no interval anyone stated; a document that
/// stated one has answered the objection.
#[test]
fn measured_gaps_pace_a_class_that_has_no_ladder_of_its_own() {
    let ack = INVITE
        .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "ACK sip:bob@10.0.0.2 SIP/2.0")
        .replace("CSeq: 1 INVITE", "CSeq: 1 ACK");
    assert!(ladder_of(&ack).is_err(), "an ACK rides no timer of its own");
    let ladder = schedule_of(ack.as_bytes(), &[203]).expect("stated gaps pace it");
    assert_eq!(ladder.interval(1), Some(ms(203)));
}

#[test]
fn an_invite_doubles_without_a_ceiling() {
    let ladder = ladder_of(INVITE).expect("an INVITE has Timer A");
    assert_eq!(ladder, Schedule::rfc(Class::InviteClient));
    assert_eq!(ladder.interval(1), Some(ms(500)));
    assert_eq!(ladder.interval(2), Some(ms(1_000)));
    assert_eq!(ladder.interval(3), Some(ms(2_000)));
    assert_eq!(ladder.interval(6), Some(ms(16_000)), "Timer A never caps");
}

#[test]
fn every_other_ladder_doubles_into_t2() {
    let bye = INVITE
        .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "BYE sip:bob@10.0.0.2 SIP/2.0")
        .replace("CSeq: 1 INVITE", "CSeq: 2 BYE");
    let ladder = ladder_of(&bye).expect("a non-INVITE request has Timer E");
    assert_eq!(ladder, Schedule::rfc(Class::NonInviteClient));
    assert_eq!(ladder.interval(1), Some(ms(500)));
    assert_eq!(ladder.interval(4), Some(ms(4_000)));
    assert_eq!(ladder.interval(9), Some(ms(4_000)), "T2 is the ceiling");
    let cancel = INVITE
        .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "CANCEL sip:bob@10.0.0.2 SIP/2.0")
        .replace("CSeq: 1 INVITE", "CSeq: 1 CANCEL");
    assert_eq!(
        ladder_of(&cancel).expect("ADR-0043 X4"),
        Schedule::rfc(Class::CancelClient),
        "a CANCEL is the class of its own the schedule names"
    );
}

#[test]
fn a_final_response_rides_the_capped_ladder() {
    let ok = "SIP/2.0 200 OK\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
            From: <sip:alice@10.0.0.1>;tag=a1\r\n\
            To: <sip:bob@10.0.0.2>;tag=b1\r\n\
            Call-ID: c1\r\n\
            CSeq: 1 INVITE\r\n\
            Content-Length: 0\r\n\r\n";
    assert_eq!(ladder_of(ok).expect("§13.3.1.4"), Schedule::rfc(Class::Final2xx));
    let busy = ok.replace("SIP/2.0 200 OK", "SIP/2.0 486 Busy Here");
    assert_eq!(ladder_of(&busy).expect("§17.2.1 Timer G"), Schedule::rfc(Class::InviteServerFinal));
    let to_bye = ok.replace("CSeq: 1 INVITE", "CSeq: 2 BYE");
    let why = ladder_of(&to_bye).expect_err("§17.2.2: re-sent on a repeated request only");
    assert!(why.contains("17.2.2"), "{why}");
    assert_eq!(
        schedule_of(to_bye.as_bytes(), &[500]).expect("stated gaps pace it").interval(1),
        Some(ms(500)),
        "a document that stated the gaps has answered the objection"
    );
}

#[test]
fn an_ack_has_no_ladder_and_says_why() {
    let ack = INVITE
        .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "ACK sip:bob@10.0.0.2 SIP/2.0")
        .replace("CSeq: 1 INVITE", "CSeq: 1 ACK");
    let why = ladder_of(&ack).expect_err("an ACK retransmits on no timer");
    assert!(why.contains("one per 2xx"), "{why}");
}

#[test]
fn an_unreliable_provisional_has_no_ladder_and_a_reliable_one_does() {
    let ringing = "SIP/2.0 180 Ringing\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1\r\n\
            From: <sip:alice@10.0.0.1>;tag=a1\r\n\
            To: <sip:bob@10.0.0.2>;tag=b1\r\n\
            Call-ID: c1\r\n\
            CSeq: 1 INVITE\r\n\
            Content-Length: 0\r\n\r\n";
    let why = ladder_of(ringing).expect_err("no RSeq, no ladder");
    assert!(why.contains("RSeq"), "{why}");
    let reliable = ringing.replace("Call-ID: c1", "RSeq: 1\r\nCall-ID: c1");
    assert_eq!(
        ladder_of(&reliable).expect("RFC 3262 §3"),
        Schedule::rfc(Class::ReliableProvisional)
    );
}

#[test]
fn a_send_ladder_counts_only_what_the_document_declared() {
    let mut repeats = Repeats::new();
    repeats.claim("s1", "A", LadderSide::Send, Some(2), &[], b"INVITE\r\n", 0);
    repeats.claim("s3", "B", LadderSide::Send, None, &[], b"OTHER\r\n", 0);
    assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
    assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
    // The same bytes on ANOTHER leg are another leg's business.
    assert_eq!(repeats.note("B", b"INVITE\r\n", 0), None);
    // Our own peer emits what the document says and nothing else, so an
    // undeclared SEND counts nothing.
    assert_eq!(repeats.note("B", b"OTHER\r\n", 0), None);
    assert!(repeats.mismatches(0).is_empty(), "2 declared, 2 seen");
    assert_eq!(
        repeats.notes(0),
        vec![RetransmitNote {
            step: "s1".into(),
            leg: "A".into(),
            side: LadderSide::Send,
            declared: 2,
            observed: 2,
            intervals_ms: Vec::new(),
            dwell_us: None,
            rfc_rungs: None
        }],
        "the verdict lists the declared ladder and what it saw"
    );
}

/// The SUT's own rungs are owed whether or not the capture held them: a
/// paced-class EXPECT claims its ladder with nothing declared, so a rung
/// that arrives is counted here instead of reaching the gate as a datagram
/// nobody expected.
#[test]
fn an_undeclared_paced_expect_counts_its_rungs_and_is_held_to_the_rfc() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
    let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s4", "B", LadderSide::Expect, None, &[], bye, 0);
    assert_eq!(repeats.note("B", bye, 500_000).as_deref(), Some("s4"));
    assert_eq!(repeats.note("B", bye, 1_500_000).as_deref(), Some("s4"));
    repeats.answered("B", ok, 1_508_000);
    assert!(
        repeats.mismatches(2_000_000).is_empty(),
        "500 and 1500 are the two rungs a 1508 ms dwell owes"
    );
    let note = &repeats.notes(2_000_000)[0];
    assert_eq!((note.declared, note.observed, note.rfc_rungs), (0, 2, Some(2)));
    // A ladder the run ASSERTS states itself, so no failure names a note
    // the verdict does not carry: nothing declared, nothing seen, 5 owed.
    let mut silent = Repeats::new();
    silent.claim("s8", "A", LadderSide::Expect, None, &[], bye, 0);
    assert_eq!(silent.notes(12_400_000)[0].rfc_rungs, Some(5));
    assert!(matches!(
        silent.mismatches(12_400_000).as_slice(),
        [Failure::RetransmitCountMismatch { declared: 0, expected: 5, observed: 0, .. }]
    ));
}

/// The emitter stops on the closer it RECEIVED, which is one hop after we
/// sent it, so a rung already on the wire arrives just behind our answer.
#[test]
fn one_rung_that_raced_the_closer_is_the_boundary_and_a_later_one_is_not() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
    let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nCSeq: 7 BYE\r\n\r\n";
    let ladder = |arrival_us: u64| {
        let mut repeats = Repeats::new();
        repeats.claim("s4", "B", LadderSide::Expect, None, &[], bye, 0);
        // A 600 ms dwell owes exactly the 500 ms rung.
        repeats.note("B", bye, 500_000);
        repeats.answered("B", ok, 600_000);
        repeats.note("B", bye, arrival_us);
        repeats.mismatches(2_000_000)
    };
    assert!(ladder(601_000).is_empty(), "1 ms behind the closer is the same rung");
    assert!(
        matches!(
            ladder(650_000).as_slice(),
            [Failure::RetransmitCountMismatch { expected: 1, observed: 2, .. }]
        ),
        "50 ms behind it is a retransmission after the answer"
    );
}

/// An ACK rides no timer: the core owes one per 2xx it received, so the
/// paired final's OWN repeats state the count, not the capture's.
#[test]
fn an_auto_ack_ladder_is_held_to_the_finals_own_repeats() {
    let ok = b"SIP/2.0 200 OK\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 INVITE\r\n\r\n";
    let ack = b"ACK sip:b@h SIP/2.0\r\nTo: <sip:b@h>;tag=b1\r\nCall-ID: c9\r\nCSeq: 4 ACK\r\n\r\n";
    let drawn = |copies: u32| {
        let mut repeats = Repeats::new();
        repeats.claim("s1", "A", LadderSide::Expect, None, &[], ok, 0);
        repeats.note("A", ok, 500_000);
        repeats.note("A", ok, 1_500_000);
        // The document read ONE repeat off the capture; the wire drew two.
        repeats.claim("s2", "A", LadderSide::Send, Some(1), &[], ack, 0);
        for _ in 0..copies {
            repeats.note("A", ack, 0);
        }
        repeats.mismatches(1_600_000)
    };
    assert!(drawn(2).is_empty(), "one ACK per repeat of the final it acknowledges");
    assert!(matches!(
        drawn(1).as_slice(),
        [Failure::RetransmitCountMismatch { step, expected: 2, observed: 1, .. }] if step == "s2"
    ));
}

#[test]
fn a_ladder_that_did_not_arrive_is_a_failure_that_names_both_counts() {
    let mut repeats = Repeats::new();
    repeats.claim("s2", "A", LadderSide::Send, Some(2), &[], b"SIP/2.0 100 Trying\r\n", 0);
    repeats.note("A", b"SIP/2.0 100 Trying\r\n", 0);
    let mismatches = repeats.mismatches(0);
    assert_eq!(
        mismatches,
        vec![Failure::RetransmitCountMismatch {
            step: "s2".into(),
            leg: "A".into(),
            declared: 2,
            expected: 2,
            observed: 1
        }]
    );
    assert_eq!(repeats.declared_for("s2"), 2);
    assert_eq!(repeats.declared_for("s9"), 0, "a step with no ladder declares none");
}

fn drawn(step: &str) -> DrawnAck {
    DrawnAck {
        step: step.into(),
        leg: "A".into(),
        wire: b"ACK\r\n".to_vec(),
        dst: "127.0.0.1:5060".parse().unwrap(),
    }
}

#[test]
fn a_repeat_sighted_before_the_ack_went_out_is_drawn_when_it_does() {
    let key = ("c1".to_string(), 1, "b1".to_string());
    let mut acks = DrawnAcks::new();
    // Two copies of the final arrive while the ACK is held.
    assert!(acks.repeat_sighted(key.clone()).is_none());
    assert!(acks.repeat_sighted(key.clone()).is_none());
    assert_eq!(acks.sent(key.clone(), drawn("s8")), 2, "both are owed at emission");
    // And a third repeat, after the hold, copies the datagram already sent.
    let copy = acks.repeat_sighted(key).expect("the ACK is out, so the repeat draws it");
    assert_eq!((copy.step.as_str(), copy.wire.as_slice()), ("s8", b"ACK\r\n".as_slice()));
}

#[test]
fn another_dialog_s_final_draws_nothing() {
    let mut acks = DrawnAcks::new();
    acks.sent(("c1".into(), 1, "b1".into()), drawn("s8"));
    // A forked 2xx confirms its own dialog: same Call-ID and CSeq, other tag.
    assert!(acks.repeat_sighted(("c1".into(), 1, "b2".into())).is_none());
    // As does the next transaction on the same dialog.
    assert!(acks.repeat_sighted(("c1".into(), 2, "b1".into())).is_none());
}

#[test]
fn one_repeat_too_many_is_counted_and_reported_not_dropped() {
    let mut repeats = Repeats::new();
    repeats.claim("s1", "A", LadderSide::Send, Some(1), &[], b"INVITE\r\n", 0);
    for _ in 0..3 {
        assert_eq!(repeats.note("A", b"INVITE\r\n", 0).as_deref(), Some("s1"));
    }
    assert!(matches!(
        repeats.mismatches(0).as_slice(),
        [Failure::RetransmitCountMismatch { declared: 1, expected: 1, observed: 3, .. }]
    ));
}

/// A captured BYE ladder, to the millisecond: the SUT's BYE ladder on
/// T1 = 500 puts a second rung on the wire 8 ms before the callee's 200,
/// where the captured platform on a 700 ms T1 fired only one. The SUT paced
/// this ladder, so the RFC states its length and the run HOLDS — the
/// document's count stays beside it as the evidence it is.
#[test]
fn an_expect_ladder_is_held_to_the_rfcs_rungs_not_the_documents_count() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
    let ok = b"SIP/2.0 200 OK\r\nCSeq: 7 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    // s15 matched the original at 30 000 ms; the two rungs are the repeats.
    repeats.claim("s15", "B", LadderSide::Expect, Some(1), &[700], bye, 30_000_000);
    repeats.note("B", bye, 30_500_000);
    repeats.note("B", bye, 31_500_000);
    repeats.answered("B", ok, 31_508_000);
    assert!(
        repeats.mismatches(40_000_000).is_empty(),
        "the SUT paced it, so the oracle is the RFC and 2 is what it owed"
    );
    assert_eq!(
        repeats.notes(40_000_000),
        vec![RetransmitNote {
            step: "s15".into(),
            leg: "B".into(),
            side: LadderSide::Expect,
            declared: 1,
            observed: 2,
            intervals_ms: vec![700],
            dwell_us: Some(1_508_000),
            rfc_rungs: Some(2)
        }],
        "T1 = 500 doubling into T2 puts 500 and 1500 inside a 1508 ms dwell"
    );
}

/// A ladder nothing closed ran until the run stopped. Judging it against the
/// capture instead would make the same SUT behaviour pass or fail on how far
/// the run got before it died.
#[test]
fn a_ladder_nothing_closed_is_owed_the_rungs_the_run_left_room_for() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[638], bye, 0);
    repeats.note("B", bye, 500_000);
    repeats.note("B", bye, 1_500_000);
    let note = &repeats.notes(1_600_000)[0];
    assert_eq!((note.dwell_us, note.rfc_rungs), (None, Some(2)));
    assert!(repeats.mismatches(1_600_000).is_empty(), "a 1.6 s run holds 500 and 1500");
    assert!(
        matches!(
            repeats.mismatches(600_000).as_slice(),
            [Failure::RetransmitCountMismatch { expected: 1, observed: 2, .. }]
        ),
        "a run that stopped at 600 ms left room for one"
    );
}

#[test]
fn a_class_that_rides_no_timer_of_its_own_states_no_rung_count() {
    let ack = b"ACK sip:b@h SIP/2.0\r\nCSeq: 7 ACK\r\n\r\n";
    let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[], ack, 0);
    repeats.note("B", ack, 0);
    repeats.note("B", ack, 0);
    // The BYE closes nothing here; the dwell comes from the ACK's own 2xx.
    repeats.answered("B", bye, 1_000_000);
    let note = &repeats.notes(2_000_000)[0];
    assert_eq!(note.rfc_rungs, None, "an ACK is drawn by the wire, never paced");
}

#[test]
fn a_reliable_provisionals_ladder_never_reaches_a_t2_ceiling() {
    // RFC 3262 §3: T1, doubling, and no T2 cap — the section names the
    // difference from a 2xx, whose ACK is drawn by the 2xx itself.
    let prack = Schedule::rfc(Class::ReliableProvisional);
    let ack = Schedule::rfc(Class::Final2xx);
    // Identical while doubling stays under T2.
    assert_eq!(rungs_within(&prack, ms(7_501)), 4);
    assert_eq!(rungs_within(&ack, ms(7_501)), 4);
    // The 5th rung is where the ceiling shows: 11.5 s capped, 15.5 s not.
    assert_eq!(rungs_within(&ack, ms(11_501)), 5);
    assert_eq!(rungs_within(&prack, ms(11_501)), 4);
}

#[test]
fn the_rung_count_walks_the_class_ladder_and_stops_at_the_closer() {
    let bye = "BYE sip:b@h SIP/2.0\r\nCSeq: 7 BYE\r\n\r\n";
    let class = class_of(bye.as_bytes()).expect("a request has Timer E");
    assert_eq!(class, Class::NonInviteClient);
    let capped = Schedule::rfc(class);
    assert_eq!(rungs_within(&capped, ms(499)), 0);
    assert_eq!(rungs_within(&capped, ms(500)), 0, "a rung AT the closer is not inside it");
    assert_eq!(rungs_within(&capped, ms(501)), 1);
    assert_eq!(rungs_within(&capped, ms(1_508)), 2);
    assert_eq!(rungs_within(&capped, ms(3_508)), 3);
    // A measured ladder EXTRAPOLATES its last gap ([`Schedule::exact`]),
    // so it states how a peer PACES a count and never how many rungs the
    // capture held: that count is the document's `retransmits` itself.
    let measured = schedule_of(bye.as_bytes(), &[700]).expect("stated gaps pace it");
    assert_eq!(rungs_within(&measured, ms(1_508)), 2);
    // And a class that rides no timer counts none: `once`, not "by the RFC".
    let ack = b"ACK sip:b@h SIP/2.0\r\nCSeq: 7 ACK\r\n\r\n";
    assert_eq!(pacing_of(ack), Schedule::once());
    assert_eq!(rungs_within(&pacing_of(ack), ms(40_000)), 0);
}

/// RFC 3262 §3 — a reliable provisional's ladder ceases on its PRACK, not
/// on the ACK that ends the INVITE it rides. The PRACK carries its own CSeq
/// and names its target in `RAck` (§7.2), so a CSeq comparison never finds it.
#[test]
fn a_reliable_provisional_closes_on_its_prack_not_on_the_ack() {
    let p183 = b"SIP/2.0 183 Session Progress\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\nRSeq: 625707\r\n\r\n";
    let prack = b"PRACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 2 PRACK\r\nRAck: 625707 1 INVITE\r\n\r\n";
    let ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 ACK\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s6", "A", LadderSide::Expect, Some(1), &[500], p183, 2_314_000);
    repeats.answered("A", prack, 2_359_000);
    // The ACK arrives five seconds later and must not move the dwell.
    repeats.answered("A", ack, 7_557_000);
    assert_eq!(
        (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
        (Some(45_000), Some(0)),
        "the PRACK closed it at 45 ms, so the T1 ladder owed no rung"
    );
}

/// A PRACK naming another INVITE's `RSeq` space closes nothing: §7.2's
/// `RAck` names its target by response-num AND the INVITE's CSeq-num.
#[test]
fn a_prack_for_another_invite_closes_nothing() {
    let p183 = b"SIP/2.0 183 Session Progress\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\nRSeq: 7\r\n\r\n";
    let elsewhere = b"PRACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 9 PRACK\r\nRAck: 7 4 INVITE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s6", "A", LadderSide::Expect, Some(1), &[], p183, 0);
    repeats.answered("A", elsewhere, 1_000_000);
    assert_eq!(repeats.notes(RUN_END)[0].dwell_us, None, "a coincidental RSeq is not an answer");
}

/// RFC 3261 §17.1.2.1 — a provisional moves a non-INVITE transaction to
/// Proceeding and RESETS Timer E to T2. It slows the ladder; only a final
/// stops it.
#[test]
fn a_provisional_does_not_close_a_non_invite_request() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
    let trying = b"SIP/2.0 100 Trying\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
    let gone = b"SIP/2.0 481 Unknown Dialog\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 77 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s47", "B", LadderSide::Expect, Some(2), &[681, 3251], bye, 2_257_923_000);
    repeats.answered("B", trying, 2_261_466_000);
    repeats.answered("B", gone, 2_265_854_000);
    assert_eq!(
        (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
        (Some(7_931_000), Some(4)),
        "the 481 closed it, and 500/1500/3500/7500 all fall inside 7931 ms"
    );
}

/// RFC 3261 §17.1.1.2 — Timer A runs only in Calling, which the FIRST
/// response of any kind leaves. An INVITE's ladder therefore closes on a
/// provisional where a BYE's does not.
#[test]
fn any_response_closes_an_invite_request() {
    let invite = b"INVITE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>\r\n\
            CSeq: 1 INVITE\r\n\r\n";
    let trying = b"SIP/2.0 100 Trying\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=t1\r\n\
            CSeq: 1 INVITE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s3", "B", LadderSide::Expect, Some(1), &[], invite, 4_000);
    repeats.answered("B", trying, 7_915_000);
    assert_eq!(
        (repeats.notes(RUN_END)[0].dwell_us, repeats.notes(RUN_END)[0].rfc_rungs),
        (Some(7_911_000), Some(4)),
        "Timer A stops in Proceeding; 500/1500/3500/7500 fall inside 7911 ms"
    );
}

/// A CSeq number is unique only within a dialog, so a fork's other early
/// dialog — same Call-ID, same CSeq, another To-tag — closes nothing (§6.5).
#[test]
fn another_early_dialogs_answer_closes_nothing() {
    let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork1\r\n\
            CSeq: 1 INVITE\r\n\r\n";
    let other_ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork2\r\n\
            CSeq: 1 ACK\r\n\r\n";
    let own_ack = b"ACK sip:b@h SIP/2.0\r\nCall-ID: c1\r\nTo: <sip:b@h>;tag=fork1\r\n\
            CSeq: 1 ACK\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s20", "A", LadderSide::Expect, Some(1), &[], ok, 0);
    repeats.answered("A", other_ack, 500_000);
    assert_eq!(repeats.notes(RUN_END)[0].dwell_us, None, "the other fork's ACK is not ours");
    repeats.answered("A", own_ack, 900_000);
    assert_eq!(repeats.notes(RUN_END)[0].dwell_us, Some(900_000));
}

/// RFC 3261 §17.2.2 — a non-INVITE server transaction re-sends its final
/// only when a retransmission of the REQUEST arrives, so that final rides no
/// timer of its own and its repeats are drawn, not paced.
#[test]
fn a_non_invite_final_rides_no_ladder_of_its_own() {
    let to_bye = b"SIP/2.0 200 OK\r\nCSeq: 77 BYE\r\n\r\n";
    let to_invite = b"SIP/2.0 200 OK\r\nCSeq: 1 INVITE\r\n\r\n";
    assert_eq!(class_of(to_bye), None, "§17.2.2 draws it from the request");
    assert_eq!(class_of(to_invite), Some(Class::Final2xx), "§13.3.1.4 paces an INVITE's 2xx");
}

/// Past 64·T1 the transaction is gone (§17.2.1 Timer H), so a ladder nothing
/// closed inside the envelope stops counting there.
#[test]
fn the_rung_count_stops_at_the_transaction_envelope() {
    let ladder = Schedule::rfc(Class::Final2xx);
    assert_eq!(
        rungs_within(&ladder, ms(31_500)),
        9,
        "500/1500/3500/7500/11500/15500/19500/23500/27500 fall inside 31.5 s"
    );
    assert_eq!(
        rungs_within(&ladder, Duration::from_secs(44)),
        10,
        "the envelope bounds it at 32 s, where a 44 s dwell would say 13"
    );
}

#[test]
fn only_the_laddered_transactions_own_answer_closes_it() {
    let bye = b"BYE sip:b@h SIP/2.0\r\nCSeq: 4 BYE\r\n\r\n";
    let other = b"SIP/2.0 200 OK\r\nCSeq: 4 INVITE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s9", "B", LadderSide::Expect, Some(1), &[], bye, 0);
    repeats.note("B", bye, 500_000);
    repeats.note("B", bye, 1_500_000);
    // Same leg, same CSeq NUMBER, another transaction: it closes nothing.
    repeats.answered("B", other, 1_508_000);
    assert_eq!(
        repeats.notes(1_600_000)[0].dwell_us,
        None,
        "a coincidental CSeq is not an answer, so nothing closed this ladder"
    );
}

/// A CANCEL's ladder ends with the INVITE it names: once that INVITE has
/// its final the CANCEL is moot (RFC 3261 §9.1) and the system stops
/// retransmitting it (ADR-0043 X4). The final our side sends to that
/// INVITE closes the CANCEL's window, so no Timer E rung is owed past it.
#[test]
fn the_final_to_the_invite_a_cancel_names_closes_the_cancels_ladder() {
    let cancel = b"CANCEL sip:b@h SIP/2.0\r\nCall-ID: c1\r\nFrom: <sip:a@h>;tag=a1\r\n\
            To: <sip:b@h>\r\nCSeq: 1 CANCEL\r\n\r\n";
    let answer = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nFrom: <sip:a@h>;tag=a1\r\n\
            To: <sip:b@h>;tag=b1\r\nCSeq: 1 INVITE\r\n\r\n";
    let mut repeats = Repeats::new();
    // The system's CANCEL, never answered; our 200 to its INVITE 319 ms on.
    repeats.claim("s10", "B", LadderSide::Expect, None, &[], cancel, 8_036_000);
    repeats.answered("B", answer, 8_355_000);
    assert!(
        repeats.mismatches(8_600_000).is_empty(),
        "the INVITE's final at 319 ms ended the ladder before its 500 ms rung: {:#?}",
        repeats.mismatches(8_600_000)
    );
}

/// The 2xx to our INITIAL INVITE is the dialog-creating one: its ladder
/// runs until the ACK or the 64·T1 give-up (RFC 3261 §13.3.1.4), and a BYE
/// its sender puts on the dialog meanwhile stops nothing — the rungs inside
/// the window stay owed.
#[test]
fn the_emitters_bye_does_not_close_a_dialog_creating_2xx_ladder() {
    let invite = b"INVITE sip:s@h SIP/2.0\r\nCall-ID: c1\r\nFrom: <sip:a@h>;tag=a1\r\n\
            To: <sip:s@h>\r\nCSeq: 1 INVITE\r\n\r\n";
    let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nFrom: <sip:a@h>;tag=a1\r\n\
            To: <sip:s@h>;tag=s1\r\nCSeq: 1 INVITE\r\n\r\n";
    let bye = b"BYE sip:a@h SIP/2.0\r\nCall-ID: c1\r\nFrom: <sip:s@h>;tag=s1\r\n\
            To: <sip:a@h>;tag=a1\r\nCSeq: 2 BYE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s1", "A", LadderSide::Send, None, &[], invite, 0);
    // The system's 2xx to our INVITE, never ACKed as captured; its BYE 300 ms on.
    repeats.note("A", ok, 100_000);
    repeats.claim("s2", "A", LadderSide::Expect, None, &[], ok, 100_000);
    repeats.note("A", bye, 400_000);
    repeats.claim("s3", "A", LadderSide::Expect, None, &[], bye, 400_000);
    let charged: Vec<Failure> = repeats
        .mismatches(5_000_000)
        .into_iter()
        .filter(|f| matches!(f, Failure::RetransmitCountMismatch { step, .. } if step == "s2"))
        .collect();
    assert_eq!(charged.len(), 1, "the 2xx ladder still owes its rungs: {charged:#?}");
}

/// A 2xx's ladder ends with the dialog it would confirm: a UAS that sends
/// its BYE on that dialog has ended it (RFC 3261 §15), and the system
/// stops retransmitting the 2xx there. The emitter's BYE, claimed as an
/// exec claims a matched arrival, closes the 2xx's window.
#[test]
fn the_emitters_bye_on_the_dialog_closes_its_2xx_ladder() {
    let ok = b"SIP/2.0 200 OK\r\nCall-ID: c1\r\nFrom: <sip:b@h>;tag=b1\r\n\
            To: <sip:s@h>;tag=s1\r\nCSeq: 1 INVITE\r\n\r\n";
    let bye = b"BYE sip:b@h SIP/2.0\r\nCall-ID: c1\r\nFrom: <sip:s@h>;tag=s1\r\n\
            To: <sip:b@h>;tag=b1\r\nCSeq: 2 BYE\r\n\r\n";
    let reinvite = b"INVITE sip:s@h SIP/2.0\r\nCall-ID: c1\r\nFrom: <sip:b@h>;tag=b1\r\n\
            To: <sip:s@h>;tag=s1\r\nCSeq: 1 INVITE\r\n\r\n";
    let mut repeats = Repeats::new();
    repeats.claim("s12", "B", LadderSide::Send, None, &[], reinvite, 900_000);
    // The system's 2xx to our re-INVITE, never ACKed; its BYE 415 ms on.
    repeats.note("B", ok, 917_000);
    repeats.claim("s14", "B", LadderSide::Expect, None, &[], ok, 917_000);
    repeats.note("B", bye, 1_332_000);
    repeats.claim("s17", "B", LadderSide::Expect, None, &[], bye, 1_332_000);
    let charged: Vec<Failure> = repeats
        .mismatches(5_272_000)
        .into_iter()
        .filter(|f| matches!(f, Failure::RetransmitCountMismatch { step, .. } if step == "s14"))
        .collect();
    assert!(
        charged.is_empty(),
        "the BYE at 415 ms ended the dialog before the 2xx's 500 ms rung: {charged:#?}"
    );
}
