use super::*;

fn stack(leg: &str, nonce: &str) -> LegStack {
    let addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    LegStack::new(leg, addr, addr, nonce, "", "")
}

/// A leg's dialog carries the identity [`dialog_identity`] names for its
/// leg and nonce: what a lane computes before the run.
#[test]
fn a_leg_dials_under_the_identity_its_leg_and_nonce_name() {
    let identity = dialog_identity("A", "n1");
    assert_eq!(identity.call_id, "A-n1@pivot.invalid");
    assert_eq!(identity.local_tag, "A-n1-tag");
    let leg = stack("A", "n1");
    assert_eq!(leg.call_id(), identity.call_id);
    assert_eq!(leg.dialog.local_tag, identity.local_tag);
}

/// A UAS stack that has taken an inbound INVITE, and the INVITE itself.
fn ringing(leg: &str) -> (LegStack, SipRequest) {
    let mut caller = stack("A", "r1a2b3");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    let invite = caller
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("the caller composes its INVITE");
    let mut uas = stack(leg, "r1a2b3");
    uas.learn_request(&invite);
    (uas, invite)
}

fn reliable(rseq: &str) -> Vec<TemplateHeader> {
    vec![TemplateHeader::frozen("Require", "100rel"), TemplateHeader::frozen("RSeq", rseq)]
}

/// A caller stack that has put its INVITE on the wire.
fn calling() -> LegStack {
    calling_with_cseq(None)
}

fn calling_with_cseq(cseq: Option<u32>) -> LegStack {
    let mut uac = stack("A", "r1a2b3");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    uac.out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, cseq)
        .expect("the caller composes its INVITE");
    uac
}

fn to_tag(response: &SipResponse) -> String {
    response.to().tag().expect("a response under a dialog carries a To-tag").to_string()
}

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

/// RFC 3262 §7.2: the RAck is the provisional's own RSeq and the CSeq of the
/// request it answers, read off the message that arrived.
#[test]
fn a_prack_acknowledges_the_provisional_the_leg_received() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let ringing_at_uas = uas
        .respond(
            &Answer {
                status: 183,
                reason: "Session Progress",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("the callee rings reliably");
    uac.learn_response(&ringing_at_uas);
    assert_eq!(uac.unacknowledged().len(), 1);
    let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs");
    assert_eq!(prack.method(), Method::Prack);
    assert_eq!(
        prack.raw_text(sip_message::HeaderName::RAck).next().map(|s| s.to_string()),
        Some("7001 1 INVITE".to_string())
    );
    assert!(uac.unacknowledged().is_empty(), "an acknowledged provisional is not PRACKed twice");
}

/// A document that froze its own RAck replays it: a frozen header always
/// wins over what the stack would compose.
#[test]
fn a_frozen_rack_wins_over_the_composed_one() {
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
            &reliable("7001"),
            Vec::new(),
            None,
        )
        .expect("the callee rings reliably");
    uac.learn_response(&rings);
    let prack = uac
        .prack(None, &[TemplateHeader::frozen("RAck", "9 9 INVITE")], Vec::new(), None, None)
        .expect("the caller PRACKs");
    let racks: Vec<String> =
        prack.raw_text(sip_message::HeaderName::RAck).map(|s| s.to_string()).collect();
    assert_eq!(racks, ["9 9 INVITE"]);
}

#[test]
fn a_prack_with_nothing_to_acknowledge_is_refused_by_name() {
    let mut uac = calling();
    assert_eq!(
        uac.prack(None, &[], Vec::new(), None, None),
        Err(StackError::NoReliableProvisional { leg: "A".into() })
    );
}

/// A caller whose callee rang two reliable forks: `f1` under RSeq 1, then
/// `f2` under RSeq 7001, both outstanding.
fn called_by_two_forks() -> LegStack {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    for (status, reason, tag, rseq) in
        [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
    {
        let rings = uas
            .respond(
                &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                &reliable(rseq),
                Vec::new(),
                None,
            )
            .expect("a fork rings");
        uac.learn_response(&rings);
    }
    uac
}

/// RFC 3261 §12.2.1.1: each early dialog one INVITE creates carries its own
/// local sequence, seeded by that INVITE — so the next request in EACH fork
/// is CSeq 2, and two forks legitimately carry the same number.
#[test]
fn two_forks_of_one_leg_number_their_own_pracks_from_the_invite_s_cseq() {
    let mut uac = called_by_two_forks();
    let first = uac.prack(Some("B-early-f1"), &[], Vec::new(), None, None).expect("fork 1's PRACK");
    let second =
        uac.prack(Some("B-early-f2"), &[], Vec::new(), None, None).expect("fork 2's PRACK");
    assert_eq!(first.to().tag(), Some("B-early-f1"));
    assert_eq!(second.to().tag(), Some("B-early-f2"));
    assert_eq!(first.cseq().seq(), 2, "fork 1's INVITE was CSeq 1");
    assert_eq!(second.cseq().seq(), 2, "fork 2's own space, not fork 1's leavings");
}

/// RFC 3262 §7.2 under interleaving: a PRACK acknowledges the oldest
/// un-acknowledged provisional OF ITS FORK, so the To-tag and the RAck
/// name one dialog whatever order the forks rang in.
#[test]
fn a_prack_acknowledges_the_oldest_provisional_of_its_own_fork() {
    let mut uac = called_by_two_forks();
    let rack = |request: &SipRequest| {
        request.raw_text(sip_message::HeaderName::RAck).next().map(|s| s.to_string())
    };
    let late = uac
        .prack(Some("B-early-f2"), &[], Vec::new(), None, None)
        .expect("fork 2 is PRACKed first");
    assert_eq!(late.to().tag(), Some("B-early-f2"));
    assert_eq!(rack(&late), Some("7001 1 INVITE".into()), "fork 2's own RSeq");
    let early = uac
        .prack(Some("B-early-f1"), &[], Vec::new(), None, None)
        .expect("fork 1 is PRACKed across the interleave");
    assert_eq!(early.to().tag(), Some("B-early-f1"));
    assert_eq!(rack(&early), Some("1 1 INVITE".into()), "fork 1's own RSeq");
    assert!(uac.unacknowledged().is_empty(), "each fork's provisional is acknowledged once");
}

/// A step naming no fork on a leg ringing ONE keeps the leg's ladder: the
/// oldest provisional overall, the fork's tag, and consecutive CSeqs.
#[test]
fn a_prack_naming_no_fork_on_a_leg_ringing_one_keeps_the_leg_s_ladder() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let prack_next = |uac: &mut LegStack, uas: &mut LegStack, rseq: &str| {
        let rings = uas
            .respond(
                &Answer {
                    status: 180,
                    reason: "Ringing",
                    cseq_method: Some("INVITE"),
                    early_tag: Some("B-early-f1"),
                },
                &reliable(rseq),
                Vec::new(),
                None,
            )
            .expect("the fork rings");
        uac.learn_response(&rings);
        uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs")
    };
    let first = prack_next(&mut uac, &mut uas, "7001");
    assert_eq!(first.cseq().seq(), 2);
    assert_eq!(first.to().tag(), Some("B-early-f1"));
    let second = prack_next(&mut uac, &mut uas, "7002");
    assert_eq!(second.cseq().seq(), 3, "one fork is one ladder");
    assert_eq!(second.to().tag(), Some("B-early-f1"));
}

/// A fork that rings AFTER another fork's PRACK exchange still seeds its
/// local sequence from the INVITE (RFC 3261 §12.2.1.1): the 200 to fork
/// 1's PRACK advances nothing in fork 2's space, so fork 2's PRACK is
/// CSeq 2, not 3.
#[test]
fn a_fork_ringing_after_another_s_prack_still_seeds_from_the_invite() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let rings = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("70789988"),
            Vec::new(),
            None,
        )
        .expect("fork 1 rings");
    uac.learn_response(&rings);
    let first = uac.prack(None, &[], Vec::new(), None, None).expect("fork 1 is PRACKed");
    assert_eq!(first.cseq().seq(), 2);
    uas.learn_request(&first);
    let prack_ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("PRACK"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("fork 1's PRACK is answered");
    uac.learn_response(&prack_ok);
    let rings_again = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &reliable("43886616"),
            Vec::new(),
            None,
        )
        .expect("fork 2 rings after the exchange");
    uac.learn_response(&rings_again);
    let second = uac.prack(None, &[], Vec::new(), None, None).expect("fork 2 is PRACKed");
    assert_eq!(second.to().tag(), Some("B-early-f2"));
    assert_eq!(second.cseq().seq(), 2, "fork 2's INVITE was its CSeq 1");
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

#[test]
fn a_prack_naming_a_fork_the_leg_never_rang_is_refused_by_name() {
    let mut uac = called_by_two_forks();
    assert_eq!(
        uac.prack(Some("B-early-f9"), &[], Vec::new(), None, None),
        Err(StackError::EarlyDialogUnknown {
            leg: "A".into(),
            method: "PRACK".into(),
            early: "B-early-f9".into(),
        })
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

/// The caller's side of the same fact: a provisional carrying a To-tag opens
/// an early dialog (RFC 3261 §12.1.2), and each fork's is its own.
#[test]
fn a_caller_opens_one_early_dialog_per_fork_that_rings_it() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    for (status, reason, tag, rseq) in
        [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
    {
        let rings = uas
            .respond(
                &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                &reliable(rseq),
                Vec::new(),
                None,
            )
            .expect("a fork rings");
        uac.learn_response(&rings);
    }
    assert_eq!(uac.early_tags().collect::<Vec<_>>(), ["B-early-f1", "B-early-f2"]);
    assert_eq!(uac.early_rseq("B-early-f1"), Some(1));
    assert_eq!(uac.early_rseq("B-early-f2"), Some(7001));
}

// ── the in-dialog drive (§6.1) ──────────────────────────────────────────

/// An established leg: the UAS took the INVITE and answered it 200, so
/// everything it exchanges from here is in-dialog.
fn established(leg: &str) -> LegStack {
    let (mut uas, _) = ringing(leg);
    uas.respond(
        &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
        &[],
        Vec::new(),
        None,
    )
    .expect("the callee answers");
    uas
}

fn header(request: &SipRequest, name: sip_message::HeaderName) -> String {
    request.raw_text(name).next().map(|s| s.to_string()).unwrap_or_default()
}

/// §6.1: an in-dialog request rides the dialog the leg ALREADY owns. A
/// re-INVITE, an UPDATE and an INFO are one dialog's traffic, not three, so
/// none of them mints a second Call-ID or a second pair of tags.
#[test]
fn every_in_dialog_request_rides_the_dialog_the_leg_already_owns() {
    let mut uas = established("B");
    let call_id = uas.call_id().to_string();
    let local_tag = uas.local_tag().to_string();
    let remote_tag = uas.remote_tag().expect("the INVITE carried a From-tag").to_string();
    for method in [Method::Invite, Method::Update, Method::Info] {
        let request = uas
            .in_dialog(method.clone(), &[], Vec::new(), None, None)
            .unwrap_or_else(|e| panic!("{method} composes in-dialog: {e}"));
        assert_eq!(request.call_id().to_string(), call_id, "{method} keeps the Call-ID");
        assert_eq!(request.from().tag(), Some(local_tag.as_str()), "{method} From-tag");
        assert_eq!(request.to().tag(), Some(remote_tag.as_str()), "{method} To-tag");
    }
}

/// RFC 3261 §12.2.1.1: each side numbers its OWN CSeq space. A leg that has
/// taken requests at the peer's numbering still counts from its own.
#[test]
fn a_leg_continues_its_own_cseq_space_whatever_the_peer_numbers() {
    let mut uas = established("B");
    assert_eq!(uas.remote_cseq(), Some(1), "the peer's INVITE was its CSeq 1");
    let first = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("an INFO");
    assert_eq!(first.cseq().seq(), 1, "this side's first request is its own CSeq 1");
    let second = uas.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(second.cseq().seq(), 2);
    assert_eq!(uas.local_cseq(), Some(2));
    // And a request the PEER sends next does not move this side's counter.
    // The peer's own INFO is composed at ITS numbering, which the document
    // never states and the two stacks never share.
    let mut peer = established("A-peer");
    peer.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("the peer's first INFO");
    let inbound =
        peer.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("the peer's second INFO");
    assert_eq!(inbound.cseq().seq(), 2, "the peer counts in its own space");
    uas.learn_request(&inbound);
    assert_eq!(uas.remote_cseq(), Some(2));
    let third = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("another INFO");
    assert_eq!(third.cseq().seq(), 3, "the peer's numbering is not this side's");
}

/// A `cseq-override` (§11) BECOMES the leg's sequence number: RFC 3261
/// §12.2.1.1 numbers every later request from the dialog's own counter, so
/// a jump the document states carries forward instead of being undone by
/// the next request.
#[test]
fn an_overridden_cseq_is_emitted_and_becomes_the_leg_s_sequence_number() {
    let mut uas = established("B");
    let jumped =
        uas.in_dialog(Method::Bye, &[], Vec::new(), None, Some(5)).expect("the dialog is alive");
    assert_eq!(jumped.cseq().seq(), 5, "the emitted request carries the override");
    assert_eq!(uas.local_cseq(), Some(5));
    let next = uas.in_dialog(Method::Info, &[], Vec::new(), None, None).expect("an INFO");
    assert_eq!(next.cseq().seq(), 6, "the leg continues from the number it emitted");
}

/// The same rule on the dialog-OPENING request: the override numbers the
/// INVITE, and the transaction it opens keeps that number.
#[test]
fn an_overridden_cseq_numbers_a_dialog_opening_invite_and_its_own_ack() {
    let mut uac = calling_with_cseq(Some(9));
    let invite = uac.sent_invite().expect("the caller dialled").clone();
    assert_eq!(invite.cseq().seq(), 9);
    let (mut uas, _) = ringing("B");
    uas.learn_request(&invite);
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    let ack = uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
    assert_eq!(ack.cseq().seq(), 9, "the ACK repeats the INVITE's number");
}

/// RFC 3261 §12.1.2: only a response to the dialog-creating request names
/// the remote tag. A 481 to the CANCEL, minted under a tag of its own, does
/// not move the dialog the 2xx confirmed: the ACK to that 2xx and the BYE
/// after it stay under the 2xx's tag.
#[test]
fn a_final_to_the_cancel_does_not_retag_the_dialog_the_2xx_confirmed() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let rings = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: None,
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee rings");
    uac.learn_response(&rings);
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    let dialog_tag = to_tag(&ok);
    assert_eq!(to_tag(&rings), dialog_tag);

    // The CANCEL crosses the 2xx and is refused by a stack that knows no
    // transaction for it, under a tag that is nobody's dialog.
    let cancel = uac.cancel(&[]).expect("the caller CANCELs");
    let mut stray = stack("B", "other");
    stray.learn_request(&cancel);
    let refused = stray
        .respond(
            &Answer {
                status: 481,
                reason: "Call/Transaction Does Not Exist",
                cseq_method: Some("CANCEL"),
                early_tag: None,
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the CANCEL is refused");
    assert_ne!(to_tag(&refused), dialog_tag, "the refusal carries a tag of its own");
    uac.learn_response(&refused);

    assert_eq!(uac.remote_tag(), Some(dialog_tag.as_str()), "the dialog keeps the 2xx's tag");
    let ack = uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs the 2xx");
    assert_eq!(ack.to().tag(), Some(dialog_tag.as_str()), "the ACK rides the 2xx's dialog");
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("the caller BYEs");
    assert_eq!(bye.to().tag(), Some(dialog_tag.as_str()), "the BYE rides the 2xx's dialog");
}

/// RFC 3261 §12.1.1, §12.2.1.1: a dialog's local and remote URIs are set
/// once, by the INVITE that created it; a re-INVITE either side sends leaves
/// them as they were. Keeping the display name and header parameters with
/// them mirrors the captured peer, which wrote its identity that way.
#[test]
fn a_re_invite_leaves_each_side_s_dialog_identity_as_the_opening_invite_set_it() {
    let alice = "\"Alice\" <sip:+15556000001;verstat=x@a.invalid;user=phone>;x-param=1";
    let bob = "\"Bob\" <sip:+15556000004@b.invalid;user=phone>";
    let mut uac = stack("A", "r1a2b3");
    let addresses = Addresses { ruri: "sip:+15556000004@b.invalid", from: alice, to: bob };
    let invite = uac
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("the caller composes its INVITE");
    let mut uas = stack("B", "r1a2b3");
    uas.learn_request(&invite);
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
    // The callee's re-INVITE writes the caller's identity without its
    // display name, which RFC 3261 permits; the caller's dialog keeps its own.
    uas.dialog.remote_uri = "<sip:+15556000001;verstat=x@a.invalid;user=phone>".to_string();
    let re_invite =
        uas.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(re_invite.to().display(), None);
    uac.learn_request(&re_invite);

    let from_caller = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("a BYE");
    assert_eq!(from_caller.from().display(), Some("Alice"));
    assert!(from_caller.from().param("x-param").is_some(), "{}", from_caller.from());
    assert_eq!(from_caller.to().display(), Some("Bob"));
    let from_callee = uas.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("a BYE");
    assert_eq!(from_callee.from().display(), Some("Bob"));
    assert_eq!(from_callee.to().display(), None, "the callee keeps the identity it wrote");
}

/// The callee side learns both addresses from the INVITE that created the
/// dialog, display names and header parameters included, and a caller
/// re-INVITE that writes them bare changes neither (RFC 3261 §12.1.1).
#[test]
fn the_callee_keeps_the_name_addrs_the_creating_invite_stated() {
    let alice = "\"Alice\" <sip:+15556000001@a.invalid;user=phone>;x-param=1";
    let bob = "\"Bob\" <sip:+15556000004@b.invalid;user=phone>";
    let mut uac = stack("A", "r9");
    let addresses = Addresses { ruri: "sip:+15556000004@b.invalid", from: alice, to: bob };
    let invite = uac
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("the caller composes its INVITE");
    let mut uas = stack("B", "r9");
    uas.learn_request(&invite);
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
    uac.dialog.local_uri = "<sip:+15556000001@a.invalid;user=phone>".to_string();
    uac.dialog.remote_uri = "<sip:+15556000004@b.invalid;user=phone>".to_string();
    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    uas.learn_request(&re_invite);

    let bye = uas.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("a BYE");
    assert_eq!(bye.from().display(), Some("Bob"));
    assert_eq!(bye.to().display(), Some("Alice"));
    assert!(bye.to().param("x-param").is_some(), "{}", bye.to());
    assert_ne!(bye.to().tag(), None, "the tag is the dialog's, kept apart");
}

/// RFC 3261 §13.2.1: the ACK to a 2xx carries the CSeq NUMBER of the INVITE
/// it answers — the re-INVITE's, not the dialog-opening one's.
#[test]
fn the_ack_to_a_re_invite_s_2xx_carries_the_re_invite_s_cseq() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    let first_ack = uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");
    assert_eq!(first_ack.cseq().seq(), 1);

    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(re_invite.cseq().seq(), 2);
    uas.learn_request(&re_invite);
    let re_ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers the re-INVITE");
    uac.learn_response(&re_ok);
    let ack =
        uac.ack_for(&re_ok, &[], Vec::new(), None, None).expect("the caller ACKs the re-INVITE");
    assert_eq!(ack.cseq().seq(), 2, "the ACK answers the transaction that was answered");
    assert_eq!(ack.method(), Method::Ack);
}

/// A `cseq-override` (§11) on the ACK to a 2xx puts the captured number on
/// the wire while the ACK still acknowledges the INVITE the response
/// answered: the transaction is found by the RESPONSE's CSeq, so the leg
/// owes no ACK afterwards and its own counter has not moved.
#[test]
fn an_overridden_cseq_reaches_the_ack_without_moving_the_leg_s_own_state() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");

    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(re_invite.cseq().seq(), 2);
    uas.learn_request(&re_invite);
    let re_ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers the re-INVITE");
    uac.learn_response(&re_ok);
    // The captured caller ACKs the re-INVITE's 2xx under the PREVIOUS
    // number, which is the defect the document states.
    let stale =
        uac.ack_for(&re_ok, &[], Vec::new(), None, Some(1)).expect("the caller ACKs the re-INVITE");
    assert_eq!(stale.cseq().seq(), 1, "the wire carries the overridden number");
    assert_eq!(stale.method(), Method::Ack);
    assert_eq!(
        uac.outstanding_invites().collect::<Vec<_>>(),
        Vec::<u32>::new(),
        "the ACK still acknowledges the INVITE the 2xx answered"
    );
    assert_eq!(uac.local_cseq(), Some(2), "an ACK numbers nothing: the counter stands");
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("the dialog ends");
    assert_eq!(bye.cseq().seq(), 3, "the leg continues from the re-INVITE, not the override");
}

/// RFC 3261 §17.1.1.3: the ACK to a non-2xx final is composed by the INVITE
/// client transaction, so a `cseq-override` on it is refused by name rather
/// than dropped — an emission that quietly kept the compliant number would
/// run green without reproducing the defect.
#[test]
fn a_cseq_override_on_the_ack_to_a_negative_final_is_refused_by_name() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let refused = uas
        .respond(
            &Answer {
                status: 486,
                reason: "Busy Here",
                cseq_method: Some("INVITE"),
                early_tag: None,
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee refuses");
    assert_eq!(
        uac.ack_for(&refused, &[], Vec::new(), None, Some(7)),
        Err(StackError::CseqOverrideOnAbsorbedAck { leg: "A".into() })
    );
}

/// RFC 3261 §13.2.1: where the INVITE carried no offer, the ANSWER rides
/// the ACK — so the step's stored body and its frozen headers go out on it.
#[test]
fn the_ack_to_a_2xx_carries_the_step_s_body_and_frozen_headers() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    let answer = b"v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\n";
    let ack = uac
        .ack_for(
            &ok,
            &[TemplateHeader::frozen("P-Charging-Vector", "icid-value=abc")],
            answer.to_vec(),
            Some("application/sdp".into()),
            None,
        )
        .expect("the caller ACKs");
    assert_eq!(ack.body(), answer.as_slice(), "the delayed-offer answer rides the ACK");
    assert_eq!(
        ack.raw_text(sip_message::HeaderName::ContentType).next().map(|v| v.to_string()),
        Some("application/sdp".to_string())
    );
    assert_eq!(
        ack.raw_text(sip_message::HeaderName::Other("P-Charging-Vector".into()))
            .next()
            .map(|v| v.to_string()),
        Some("icid-value=abc".to_string())
    );
}

/// RFC 3261 §17.1.1.3: the ACK to a NON-2xx is part of the INVITE client
/// transaction, so it reuses that INVITE's own top Via branch — the
/// re-INVITE's, which is what the absorption seam keys the obligation on.
#[test]
fn the_ack_to_an_in_dialog_negative_final_rides_the_re_invite_s_own_branch() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("INVITE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    uac.ack_for(&ok, &[], Vec::new(), None, None).expect("the caller ACKs");

    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    let branch = re_invite.top_via().branch().unwrap_or_default().to_string();
    uas.learn_request(&re_invite);
    let refused = uas
        .respond(
            &Answer {
                status: 488,
                reason: "Not Acceptable Here",
                cseq_method: Some("INVITE"),
                early_tag: None,
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the peer refuses the offer");
    let ack = uac.ack_for(&refused, &[], Vec::new(), None, None).expect("the caller ACKs the 488");
    assert_eq!(ack.top_via().branch().unwrap_or_default(), branch.as_str());
    assert_eq!(ack.cseq().seq(), re_invite.cseq().seq());
    // A negative in-dialog final closes nothing: the dialog stands and the
    // next request keeps numbering where the re-INVITE left off (RFC 3261 §12.2.1.1).
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("the dialog is alive");
    assert_eq!(bye.cseq().seq(), 3);
}

/// An in-dialog request on a leg that has no dialog is REFUSED by name.
/// Composing one would mint a second dialog identity for a leg that already
/// has one coming, which is a wrong run reported as a green one.
#[test]
fn an_in_dialog_request_on_a_leg_with_no_dialog_is_refused_by_name() {
    let mut fresh = stack("B", "r1a2b3");
    assert_eq!(
        fresh.in_dialog(Method::Info, &[], Vec::new(), None, None),
        Err(StackError::NoDialog { leg: "B".into(), method: "INFO".into() })
    );
}

/// RFC 3261 §12.1.1: a provisional the UAS sends under a To-tag opens an
/// early dialog on its side too. A BYE the callee puts on it (§15 forbids
/// it to the callee, and peers still send one) rides that dialog: the
/// leg's Call-ID, its own tag on the From, the caller's on the To. It is
/// never a dialog-opening request, which this leg cannot address.
#[test]
fn a_callee_bye_on_its_early_dialog_rides_that_dialog() {
    let (mut uas, invite) = ringing("B");
    let caller_tag = invite.from().tag().expect("the INVITE carried a From-tag").to_string();
    uas.respond(
        &Answer {
            status: 180,
            reason: "Ringing",
            cseq_method: Some("INVITE"),
            early_tag: Some("B-early-f1"),
        },
        &[],
        Vec::new(),
        None,
    )
    .expect("the callee rings under a tag");
    let bye = uas
        .in_dialog(Method::Bye, &[], Vec::new(), None, None)
        .expect("the callee BYEs its early dialog");
    assert_eq!(bye.method(), Method::Bye);
    assert_eq!(bye.from().tag(), Some("B-early-f1"), "the BYE rides the early dialog");
    assert_eq!(bye.to().tag(), Some(caller_tag.as_str()), "the peer's tag is the caller's");
    assert_eq!(bye.call_id().to_string(), uas.call_id(), "one leg, one Call-ID");
}

/// RFC 3261 §12.1.2: a provisional carrying a new Contact refreshes the
/// early dialog's remote target. After `refresh_target` the leg's Contact
/// names a user part of its own, on that message and every later one.
#[test]
fn a_target_refresh_changes_the_contact_the_leg_states() {
    let (mut uas, _) = ringing("B");
    let ring = |uas: &mut LegStack| {
        let answer =
            Answer { status: 180, reason: "Ringing", cseq_method: Some("INVITE"), early_tag: None };
        let r = uas.respond(&answer, &[], Vec::new(), None).expect("a ring");
        r.contacts().as_slice().first().expect("a 180 states a Contact").uri().to_string()
    };
    let first = ring(&mut uas);
    uas.refresh_target(1);
    let second = ring(&mut uas);
    assert_ne!(first, second, "the refreshed target is another URI");
    assert!(second.contains("B-1@"), "{second}");
    assert_eq!(ring(&mut uas), second, "the refresh holds for later messages");
}

// ── UPDATE on an early dialog (RFC 3311 §5.1) ───────────────────────────

/// A UAS ringing two forks, each under its own tag and RSeq.
fn forking(leg: &str) -> (LegStack, SipRequest) {
    let (mut uas, invite) = ringing(leg);
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
    (uas, invite)
}

const OFFER: &[u8] = b"v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
        t=0 0\r\nm=audio 20001 RTP/AVP 0\r\n";

/// RFC 3311 §5.1: an UPDATE runs INSIDE an early dialog. The fork the step
/// names is the dialog it rides — its tag on the From, the peer's on the
/// To — and the offer it carries goes with it.
#[test]
fn an_update_rides_the_early_dialog_the_step_names() {
    let (mut uas, invite) = forking("B");
    let caller_tag = invite.from().tag().expect("the INVITE carried a From-tag").to_string();
    let update = uas
        .update(Some("B-early-f2"), &[], OFFER.to_vec(), Some("application/sdp".into()), None)
        .expect("the second fork updates its own early dialog");
    assert_eq!(update.method(), Method::Update);
    assert_eq!(update.from().tag(), Some("B-early-f2"), "the UPDATE rides fork 2");
    assert_eq!(update.to().tag(), Some(caller_tag.as_str()), "the peer's tag is the caller's");
    assert_eq!(update.call_id().to_string(), uas.call_id(), "one leg, one Call-ID");
    assert_eq!(update.body().as_ref(), OFFER, "the offer rides the UPDATE");
    assert_eq!(header(&update, sip_message::HeaderName::ContentType), "application/sdp");
    // RFC 3261 §12.2.1.1: each dialog numbers its OWN local sequence, and
    // this side has sent nothing on this one before.
    assert_eq!(update.cseq().seq(), 1);
    let second = uas
        .update(Some("B-early-f2"), &[], Vec::new(), None, None)
        .expect("the fork updates again");
    assert_eq!(second.cseq().seq(), 2, "the fork continues its own numbering");
    // And the OTHER fork numbers from its own zero, not fork 2's.
    let other = uas
        .update(Some("B-early-f1"), &[], Vec::new(), None, None)
        .expect("the first fork updates its own early dialog");
    assert_eq!(other.from().tag(), Some("B-early-f1"));
    assert_eq!(other.cseq().seq(), 1);
}

/// The caller's direction of the same request: the fork is the REMOTE's tag,
/// and the CSeq continues the caller's own numbering from its INVITE.
#[test]
fn a_caller_s_update_on_its_one_early_dialog_needs_no_name() {
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
        .expect("the callee rings");
    uac.learn_response(&rings);
    let update = uac
        .update(None, &[], OFFER.to_vec(), Some("application/sdp".into()), None)
        .expect("one early dialog needs no naming");
    assert_eq!(update.method(), Method::Update);
    assert_eq!(update.to().tag(), Some("B-early-f1"), "the UPDATE reaches the fork that rang");
    assert_eq!(update.from().tag(), Some(uac.local_tag()));
    assert_eq!(update.cseq().seq(), 2, "the caller's INVITE was its CSeq 1");
}

/// §14: the tool infers nothing. A leg ringing two forks and an UPDATE
/// naming neither is a refusal — which fork it rides decides who receives
/// it — and so is a fork the leg never rang.
#[test]
fn an_update_that_cannot_tell_which_fork_it_rides_is_refused_by_name() {
    let (mut uas, _) = forking("B");
    assert_eq!(
        uas.update(None, &[], Vec::new(), None, None),
        Err(StackError::EarlyDialogAmbiguous {
            leg: "B".into(),
            method: "UPDATE".into(),
            dialogs: 2,
        })
    );
    assert_eq!(
        uas.update(Some("B-early-f9"), &[], Vec::new(), None, None),
        Err(StackError::EarlyDialogUnknown {
            leg: "B".into(),
            method: "UPDATE".into(),
            early: "B-early-f9".into(),
        })
    );
    // A leg that has rung NO fork and confirmed no dialog has no dialog at
    // all.
    let mut fresh = stack("B", "r1a2b3");
    assert_eq!(
        fresh.update(None, &[], Vec::new(), None, None),
        Err(StackError::NoDialog { leg: "B".into(), method: "UPDATE".into() })
    );
}

/// Once the dialog confirms, an UPDATE is ordinary in-dialog traffic on the
/// dialog the leg owns — the fork that answered — and numbers itself in that
/// dialog's sequence space whether or not the step still names the fork.
#[test]
fn an_update_after_confirmation_rides_the_confirmed_dialog() {
    let (mut uas, invite) = forking("B");
    let early = uas
        .update(Some("B-early-f1"), &[], Vec::new(), None, None)
        .expect("fork 1 updates before answering");
    assert_eq!(early.cseq().seq(), 1);
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
    let confirmed = uas.update(None, &[], Vec::new(), None, None).expect("the dialog is up");
    assert_eq!(confirmed.from().tag(), Some("B-early-f1"), "the fork that answered IS the dialog");
    assert_eq!(confirmed.to().tag(), invite.from().tag());
    assert_eq!(confirmed.cseq().seq(), 2, "the confirmed dialog continues the fork's numbering");
    // And the plain in-dialog path composes the same request.
    let plain = uas.in_dialog(Method::Update, &[], Vec::new(), None, None).expect("in-dialog");
    assert_eq!(plain.cseq().seq(), 3);
}

/// RFC 3261 §12.2.1.1 at confirmation: the leg continues the CONFIRMED
/// dialog's sequence. A number burned in ANOTHER fork does not carry across
/// to the fork that answered.
#[test]
fn confirmation_adopts_the_answering_fork_s_own_sequence() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    for (status, reason, tag, rseq) in
        [(183, "Session Progress", "B-early-f1", "1"), (180, "Ringing", "B-early-f2", "7001")]
    {
        let rings = uas
            .respond(
                &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
                &reliable(rseq),
                Vec::new(),
                None,
            )
            .expect("a fork rings");
        uac.learn_response(&rings);
    }
    // Fork 1 burns its CSeq 2 on an UPDATE; fork 2's space stays untouched.
    let update = uac
        .update(Some("B-early-f1"), &[], Vec::new(), None, None)
        .expect("fork 1 updates its own early dialog");
    assert_eq!(update.cseq().seq(), 2);
    let ok = uas
        .respond(
            &Answer {
                status: 200,
                reason: "OK",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("fork 2 answers");
    uac.learn_response(&ok);
    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(re_invite.cseq().seq(), 2, "fork 2's only prior request was the INVITE's CSeq 1");
}

/// RFC 3261 §12.2.1.1 across confirmation: a 2xx to the PRACK or UPDATE an
/// early dialog carries confirms nothing, so the requests the leg sends on
/// that early dialog keep advancing ITS sequence, and the 2xx to the INVITE
/// continues from the last of them — the BYE behind PRACK 2 and UPDATE 3 is 4.
#[test]
fn a_2xx_to_an_early_request_confirms_nothing_and_the_sequence_carries_on() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let rings = uas
        .respond(
            &Answer {
                status: 180,
                reason: "Ringing",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f1"),
            },
            &reliable("1"),
            Vec::new(),
            None,
        )
        .expect("the callee rings reliably");
    uac.learn_response(&rings);
    let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs");
    assert_eq!(prack.cseq().seq(), 2);
    uas.learn_request(&prack);
    let pracked = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("PRACK"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers the PRACK");
    uac.learn_response(&pracked);
    let update = uac.update(None, &[], Vec::new(), None, None).expect("the caller updates");
    assert_eq!(update.cseq().seq(), 3);
    let ok = uas
        .respond(
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
        .expect("the callee answers");
    uac.learn_response(&ok);
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("a BYE");
    assert_eq!(bye.cseq().seq(), 4, "the BYE follows the UPDATE's 3");
}

/// A confirming To-tag that rang no provisional confirms a dialog whose
/// only prior request was the INVITE, so the INVITE's own CSeq seeds the
/// sequence the leg continues — never 0 and never the leg's running count.
#[test]
fn confirmation_under_a_tag_that_rang_nothing_seeds_from_the_invite() {
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
    // The one fork burns its CSeq 2 on an UPDATE, whose 200 syncs the leg.
    let update = uac.update(None, &[], Vec::new(), None, None).expect("the fork updates");
    assert_eq!(update.cseq().seq(), 2);
    uas.learn_request(&update);
    let update_ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("UPDATE"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the UPDATE is answered");
    uac.learn_response(&update_ok);
    // The INVITE is answered under a tag no provisional carried.
    let ok = uas
        .respond(
            &Answer {
                status: 200,
                reason: "OK",
                cseq_method: Some("INVITE"),
                early_tag: Some("B-early-f2"),
            },
            &[],
            Vec::new(),
            None,
        )
        .expect("the INVITE answers under a fresh tag");
    uac.learn_response(&ok);
    let re_invite =
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None).expect("a re-INVITE");
    assert_eq!(re_invite.cseq().seq(), 2, "the confirmed dialog held only the INVITE's CSeq 1");
}

/// Before confirmation every in-dialog request rides an early dialog, so a
/// leg ringing several forks names which: unnamed, the request is refused,
/// never put on a fork the document did not choose.
#[test]
fn an_early_dialog_request_names_its_fork_where_the_leg_rings_several() {
    let (mut uas, _) = forking("B");
    assert_eq!(
        uas.in_dialog(Method::Info, &[], Vec::new(), None, None),
        Err(StackError::EarlyDialogAmbiguous {
            leg: "B".into(),
            method: "INFO".into(),
            dialogs: 2
        })
    );
    assert_eq!(
        uas.in_dialog(Method::Bye, &[], Vec::new(), None, None),
        Err(StackError::EarlyDialogAmbiguous { leg: "B".into(), method: "BYE".into(), dialogs: 2 })
    );
    let bye = uas
        .in_dialog_on(Some("B-early-f2"), Method::Bye, &[], Vec::new(), None, None)
        .expect("the named fork");
    assert_eq!(bye.from().tag(), Some("B-early-f2"));
}

/// A leg that rang nothing has no dialog to address an in-dialog request on.
#[test]
fn an_in_dialog_request_on_a_leg_that_rang_nothing_is_refused() {
    let (mut uas, _) = ringing("B");
    assert_eq!(
        uas.in_dialog(Method::Bye, &[], Vec::new(), None, None),
        Err(StackError::NoDialog { leg: "B".into(), method: "BYE".into() })
    );
}

/// A frozen body rides an in-dialog request with the media type the document
/// states: an INFO's payload is the transfer intake's whole content.
#[test]
fn an_in_dialog_request_carries_the_body_and_media_type_the_step_states() {
    let mut uas = established("B");
    let info = uas
        .in_dialog(
            Method::Info,
            &[],
            b"dwdHAIQDjwGh".to_vec(),
            Some("application/example-binary".into()),
            None,
        )
        .expect("an INFO with a frozen body");
    assert_eq!(info.body().as_ref(), b"dwdHAIQDjwGh");
    assert_eq!(header(&info, sip_message::HeaderName::ContentType), "application/example-binary");
    assert_eq!(header(&info, sip_message::HeaderName::ContentLength), "12");
}

/// Compile once, run MANY: two instances of one plan must not share a
/// dialog identity, or the second run folds into the first's state.
#[test]
fn two_runs_of_one_leg_mint_disjoint_identities() {
    let first = stack("A", "r1a2b3");
    let second = stack("A", "r9z8y7");
    assert_ne!(first.call_id(), second.call_id());
    assert_ne!(first.local_tag(), second.local_tag());
    // The leg id stays in front, so a trace still reads.
    assert!(first.call_id().starts_with("A-"), "{}", first.call_id());
    assert!(first.local_tag().starts_with("A-"), "{}", first.local_tag());
}

#[test]
fn two_legs_of_one_run_mint_disjoint_identities() {
    let a = stack("A", "r1a2b3");
    let b = stack("B", "r1a2b3");
    assert_ne!(a.call_id(), b.call_id());
    assert_ne!(a.local_tag(), b.local_tag());
}

/// §8.1.1.7: a branch is unique across TIME, not merely within a run.
/// §17.2.3 matches a server transaction on branch + sent-by + method and
/// never on the Call-ID, so two runs on one socket would otherwise merge.
#[test]
fn two_runs_mint_disjoint_via_branches() {
    let mut first = stack("A", "r1a2b3");
    let mut second = stack("A", "r9z8y7");
    let branches = |s: &mut LegStack| {
        (0..3).map(|_| s.via().branch().unwrap_or_default().to_string()).collect::<Vec<_>>()
    };
    let one = branches(&mut first);
    let two = branches(&mut second);
    assert!(one.iter().all(|b| b.starts_with("z9hG4bK")), "{one:?}");
    for branch in &one {
        assert!(!two.contains(branch), "{branch} minted by both runs");
    }
    // And each run's own branches stay distinct per transaction.
    let mut sorted = one.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), one.len(), "{one:?}");
}

/// A caller whose callee rang fork `B-f1`, then answered on fork `B-f3`,
/// which never rang; the caller has learned both. Returns the caller, the
/// callee, and the 2xx.
fn answered_on_an_unrung_fork() -> (LegStack, LegStack, SipResponse) {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let answer = |status, reason, tag| Answer {
        status,
        reason,
        cseq_method: Some("INVITE"),
        early_tag: Some(tag),
    };
    let rings =
        uas.respond(&answer(180, "Ringing", "B-f1"), &[], Vec::new(), None).expect("fork 1 rings");
    uac.learn_response(&rings);
    let ok =
        uas.respond(&answer(200, "OK", "B-f3"), &[], Vec::new(), None).expect("fork 3 answers");
    uac.learn_response(&ok);
    (uac, uas, ok)
}

/// `request` as the peer would have sent it under From-tag `from_tag`.
fn under_from_tag(request: &SipRequest, from_tag: &str) -> SipRequest {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    let wire = String::from_utf8(request.image().to_vec()).expect("text");
    let tag = request.from().tag().expect("the request carries a From-tag");
    let rewritten = wire.replacen(&format!("tag={tag}"), &format!("tag={from_tag}"), 1);
    match CustomParser::new().parse(rewritten.as_bytes()).expect("parses") {
        SipMessage::Request(r) => r,
        _ => panic!("a request"),
    }
}

/// The tag a request carries is learned only while no 2xx has confirmed the
/// dialog: a stray request leaves the caller's dialog where its 2xx put it,
/// so her own next request still names fork 3.
#[test]
fn a_confirmed_dialog_keeps_its_remote_tag_whatever_request_arrives() {
    let (mut uac, mut uas, _) = answered_on_an_unrung_fork();
    let options = uas.in_dialog(Method::Options, &[], Vec::new(), None, None).expect("OPTIONS");
    uac.learn_request(&under_from_tag(&options, "B-f1"));
    assert_eq!(uac.remote_tag(), Some("B-f3"));
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("BYE");
    assert_eq!(bye.to().tag(), Some("B-f3"));
}

/// The response `text` holds, re-read after `edit` rewrote it.
fn edited(text: &[u8], edit: impl Fn(String) -> String) -> SipResponse {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    let rewritten = edit(String::from_utf8(text.to_vec()).expect("text"));
    match CustomParser::new().parse(rewritten.as_bytes()).expect("parses") {
        SipMessage::Response(r) => r,
        _ => panic!("a response"),
    }
}

/// Once fork 3's 2xx confirmed the dialog, another fork's late provisional
/// or 2xx confirms nothing of it (RFC 3261 §12.1.2, §13.2.2.4): the leg
/// keeps the remote tag and target its own 2xx gave it.
#[test]
fn another_forks_late_response_leaves_the_confirmed_dialog_as_it_was() {
    let (mut uac, _, ok) = answered_on_an_unrung_fork();
    let target = uac.remote_target().to_string();
    let elsewhere = |text: String| {
        text.replacen("tag=B-f3", "tag=B-f1", 1).replacen(
            "Contact: <sip:",
            "Contact: <sip:elsewhere-",
            1,
        )
    };
    let late_180 =
        edited(ok.image(), |t| elsewhere(t).replacen("SIP/2.0 200 OK", "SIP/2.0 180 Ringing", 1));
    let late_200 = edited(ok.image(), elsewhere);
    for late in [late_180, late_200] {
        uac.learn_response(&late);
        assert_eq!(uac.remote_tag(), Some("B-f3"), "{}", late.status());
        assert_eq!(uac.remote_target(), target, "{}", late.status());
    }
    let bye = uac.in_dialog(Method::Bye, &[], Vec::new(), None, None).expect("BYE");
    assert_eq!(bye.to().tag(), Some("B-f3"));
}

/// A request taken inside 64·T1 on another fork's early dialog still
/// standing is that dialog's: the confirmed dialog keeps its remote target
/// and route set.
#[test]
fn a_request_on_another_forks_early_dialog_leaves_the_confirmed_dialog_as_it_was() {
    let (mut uac, mut uas, _) = answered_on_an_unrung_fork();
    let target = uac.remote_target().to_string();
    let routes = uac.route_set().to_vec();
    let update = uas.in_dialog(Method::Update, &[], Vec::new(), None, None).expect("UPDATE");
    let early = {
        use sip_message::parser::custom::CustomParser;
        use sip_message::{SipMessage, SipParser};
        let text = String::from_utf8(update.image().to_vec())
            .expect("text")
            .replacen("tag=B-f3", "tag=B-f1", 1)
            .replacen("Contact: <sip:", "Contact: <sip:elsewhere-", 1)
            .replacen("CSeq:", "Record-Route: <sip:10.9.9.9;lr>\r\nCSeq:", 1);
        match CustomParser::new().parse(text.as_bytes()).expect("parses") {
            SipMessage::Request(r) => r,
            _ => panic!("a request"),
        }
    };
    uac.learn_request(&early);
    assert_eq!(uac.remote_target(), target);
    assert_eq!(uac.route_set(), routes.as_slice());
    assert_eq!(uac.remote_tag(), Some("B-f3"));
}
