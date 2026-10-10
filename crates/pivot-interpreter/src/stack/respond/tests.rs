use crate::stack::tests::{calling, reliable, ringing, to_tag};
use crate::stack::*;

/// §6.1: each fork answers under its OWN early dialog, and two forks sharing
/// a To-tag would be one dialog to every peer.
#[test]
fn two_forks_of_one_leg_answer_under_the_tags_the_run_minted() {
    let (mut uas, _) = ringing("B");
    let first = uas
        .respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
    let second = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("fork 2 rings");
    assert_eq!(to_tag(&first), "B-early-f1");
    assert_eq!(to_tag(&second), "B-early-f2");
}

/// RFC 3261 §12.1.1: a request that already names its dialog is answered
/// under THAT tag — a PRACK for one fork is never answered under another.
#[test]
fn a_response_repeats_the_to_tag_the_request_carried() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let rings = uas
        .respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
    uac.learn_response(&rings);
    let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs fork 1");
    assert_eq!(prack.to().tag(), Some("B-early-f1"), "a PRACK rides its own early dialog");
    uas.learn_request(&prack);
    let ok = uas
        .respond(
            &Answer {
                status: 200,
                reason: "OK",
                cseq_method: Some("PRACK"),
                early_tag: Some("B-early-f2"),
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the PRACK is answered");
    assert_eq!(to_tag(&ok), "B-early-f1", "the answered request names its own dialog");
}

/// The fork that ANSWERS is the dialog that confirmed, so the leg adopts its
/// tag: everything in-dialog after it rides the dialog that was answered.
#[test]
fn the_fork_that_answers_becomes_the_leg_s_dialog() {
    let (mut uas, _) = ringing("B");
    uas.respond(
        &Answer {
            status: 183,
            reason: "Session Progress",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f1"),
        },
        &reliable("1"),
        Vec::new(),
        None,
    )
    .expect("fork 1 rings");
    assert_ne!(uas.local_tag(), "B-early-f1", "ringing confirms nothing");
    uas.respond(
        &Answer {
            status: 200,
            reason: "OK",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f1"),
        },
        &[],
        Vec::new(),
        None,
    )
    .expect("fork 1 answers");
    assert_eq!(uas.local_tag(), "B-early-f1");
}

/// RFC 3262 §3: the interpreter never invents the number a peer's RAck
/// quotes.
#[test]
fn a_reliable_provisional_without_an_rseq_is_refused_by_name() {
    let (mut uas, _) = ringing("B");
    let refused = uas.respond(
        &Answer {
            status: 183,
            reason: "Session Progress",
            cseq_method: Some("INVITE"),
            early_tag: None,
        },
        &[TemplateHeader::frozen("Require", "100rel")],
        Vec::new(),
        None,
    );
    assert_eq!(refused, Err(StackError::NoRSeq { leg: "B".into(), status: 183 }));
}

/// [`respond_to`](LegStack::respond_to) answers the transaction it names,
/// not the newest of the method: two forks' PRACKs are both CSeq 2, so the
/// To-tag is what tells fork 1's from fork 2's.
#[test]
fn respond_to_answers_the_named_transaction_not_the_newest_of_its_method() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    for (fork, rseq) in [("B-early-f1", "1"), ("B-early-f2", "1")] {
        let rings = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some(fork),
                },
                &reliable(rseq),
                Vec::new(),
                None,
            )
            .expect("the fork rings");
        uac.learn_response(&rings);
        let prack = uac.prack(Some(fork), &[], Vec::new(), None, None).expect("PRACKed");
        assert_eq!(prack.cseq().seq(), 2, "each fork's own space");
        uas.learn_request(&prack);
    }
    let answer = Answer { status: 200, reason: "OK", cseq_method: Some("PRACK"), early_tag: None };
    let first = uas
        .respond_to(2, Some("B-early-f1"), &answer, &[], Vec::new(), None)
        .expect("fork 1's PRACK is answered by name");
    assert_eq!(first.to().tag(), Some("B-early-f1"), "not the newest PRACK, fork 2's");
    assert!(
        uas.respond_to(2, Some("B-early-f9"), &answer, &[], Vec::new(), None).is_err(),
        "a transaction the leg never took is refused"
    );
}

/// `${leg:<id>.rseq}` reads the last reliable provisional the leg sighted,
/// whichever side of the wire it was on.
#[test]
fn the_leg_publishes_the_rseq_it_last_sighted() {
    let (mut uas, _) = ringing("B");
    assert_eq!(uas.rseq(), None);
    uas.respond(
        &Answer {
            status: 183,
            reason: "Session Progress",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f1"),
        },
        &reliable("1"),
        Vec::new(),
        None,
    )
    .expect("fork 1 rings");
    assert_eq!(uas.rseq(), Some(1));
    uas.respond(
        &Answer {
            status: 180,
            reason: "Ringing",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f2"),
        },
        &reliable("7001"),
        Vec::new(),
        None,
    )
    .expect("fork 2 rings");
    assert_eq!(uas.rseq(), Some(7001));
}

/// RFC 3262 §3 numbers an RSeq space PER early dialog, so each
/// fork publishes its own — which is what `${early:<id>.rseq}` reads and
/// what the leg's single value cannot say.
#[test]
fn each_fork_publishes_the_rseq_of_its_own_reliable_provisional() {
    let (mut uas, _) = ringing("B");
    uas.respond(
        &Answer {
            status: 183,
            reason: "Session Progress",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f1"),
        },
        &reliable("1"),
        Vec::new(),
        None,
    )
    .expect("fork 1 rings");
    uas.respond(
        &Answer {
            status: 180,
            reason: "Ringing",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f2"),
        },
        &reliable("7001"),
        Vec::new(),
        None,
    )
    .expect("fork 2 rings");
    assert_eq!(uas.early_rseq("B-early-f1"), Some(1));
    assert_eq!(uas.early_rseq("B-early-f2"), Some(7001));
    assert_eq!(uas.early_rseq("B-early-f9"), None);
    assert_eq!(uas.early_tags().collect::<Vec<_>>(), ["B-early-f1", "B-early-f2"]);
}

/// RFC 3262 §3: a second reliable provisional on ONE early dialog is
/// greater by exactly one, whatever unrelated number the document states —
/// a capture whose two forks the cut folded into one fork numbers here.
#[test]
fn a_second_reliable_provisional_on_one_fork_rises_by_exactly_one() {
    let (mut uas, _) = ringing("B");
    let first = uas
        .respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1318758475"),
            Vec::new(),
            None,
        )
        .expect("the fork rings");
    let second = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("705007309"),
            Vec::new(),
            None,
        )
        .expect("the fork rings again");
    assert_eq!(rseq_of(&first), Some(1318758475), "the document seeds the space");
    assert_eq!(rseq_of(&second), Some(1318758476));
    assert_eq!(uas.early_rseq("B-early-f1"), Some(1318758476));
    assert_eq!(uas.rseq(), Some(1318758476));
}

/// The same number twice is the SAME provisional retransmitted (RFC 3262
/// §3), so it keeps it and the space does not advance.
#[test]
fn a_reliable_provisional_the_document_repeats_keeps_its_number() {
    let (mut uas, _) = ringing("B");
    for _ in 0..2 {
        let rings = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable("7001"),
                Vec::new(),
                None,
            )
            .expect("the fork rings");
        assert_eq!(rseq_of(&rings), Some(7001));
    }
    assert_eq!(uas.early_rseq("B-early-f1"), Some(7001));
}
