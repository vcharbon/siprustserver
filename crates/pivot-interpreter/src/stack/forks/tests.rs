use crate::stack::tests::{calling, header, reliable, ringing, stack};
use crate::stack::*;

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

/// The callee's answer to the caller's newest INVITE, under fork `tag`.
fn answer_invite(uas: &mut LegStack, status: u16, reason: &str, tag: &str) -> SipResponse {
    uas.respond(
        &Answer { status, reason, cseq_method: Some("INVITE"), early_tag: Some(tag) },
        &[],
        Vec::new(),
        None,
    )
    .expect("the callee answers the INVITE")
}

/// RFC 3261 §12.3: a non-2xx final to an INVITE ends the early dialogs its
/// provisionals opened, so the caller rings nothing once the 487 is in. A
/// request the script still puts on that dialog is one a peer sent into a
/// dialog already ended, which the far side answers 481 (§12.2.2): it is
/// composed on what the ended dialog held, its tag, target and sequence.
#[test]
fn a_non_2xx_final_ends_the_early_dialogs_its_invite_opened() {
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
        .expect("the callee rings reliably");
    uac.learn_response(&rings);
    let target = uac.remote_target().to_string();
    let prack = uac.prack(None, &[], Vec::new(), None, None).expect("the caller PRACKs");
    assert_eq!(prack.cseq().seq(), 2);
    let cancel = uac.cancel(&[]).expect("the caller CANCELs");
    uas.learn_request(&cancel);
    let cancelled = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("CANCEL"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the CANCEL is answered");
    uac.learn_response(&cancelled);
    assert!(uac.rings_early(), "the CANCEL's 200 ends nothing");
    uac.learn_response(&answer_invite(&mut uas, 487, "Request Terminated", "B-early-f1"));
    assert!(!uac.rings_early(), "the 487 ended the early dialog");
    assert!(!uac.has_dialog());
    let bye = uac
        .in_dialog(Method::Bye, &[], Vec::new(), None, None)
        .expect("the BYE is composed on the ended dialog");
    assert_eq!(bye.to().tag(), Some("B-early-f1"));
    assert_eq!(bye.request_uri().to_string(), target);
    assert_eq!(bye.cseq().seq(), 3, "the ended dialog's sequence carries on past its PRACK");
}

/// RFC 3261 §12.3 scopes the ending to ONE INVITE: the forks its final
/// ended no longer compete with a fork an INVITE sent after it rings, so an
/// UPDATE naming none rides the one still ringing.
#[test]
fn an_ended_fork_does_not_compete_with_one_still_ringing() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    uac.learn_response(&answer_invite(&mut uas, 183, "Session Progress", "B-early-f1"));
    uac.learn_response(&answer_invite(&mut uas, 487, "Request Terminated", "B-early-f1"));
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    let offered_again = uac
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("the caller offers the call again");
    uas.learn_request(&offered_again);
    uac.learn_response(&answer_invite(&mut uas, 180, "Ringing", "B-early-f2"));
    assert!(uac.rings_early());
    let update = uac
        .update(None, &[], Vec::new(), None, None)
        .expect("the one fork still ringing needs no name");
    assert_eq!(update.to().tag(), Some("B-early-f2"));
}

/// RFC 3261 §12.3, §14: an INVITE offered again after its predecessor's
/// non-2xx final is no re-INVITE. The early dialog that final ended is no
/// dialog an INVITE may ride, so the request is refused by name rather than
/// sent under the ended fork's To-tag.
#[test]
fn an_invite_never_rides_an_ended_early_dialog() {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    uac.learn_response(&answer_invite(&mut uas, 180, "Ringing", "B-early-f1"));
    uac.learn_response(&answer_invite(&mut uas, 487, "Request Terminated", "B-early-f1"));
    assert_eq!(
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None),
        Err(StackError::NoDialog { leg: "A".into(), method: "INVITE".into() })
    );
}

/// RFC 3261 §12.3 ends the early dialogs of ONE INVITE transaction, which
/// its Call-ID and CSeq name together: a callee answering 487 to another
/// call's INVITE of the same CSeq number leaves its own fork ringing.
#[test]
fn a_final_to_another_call_s_invite_ends_none_of_this_call_s_forks() {
    let (mut uas, _) = ringing("B");
    answer_invite(&mut uas, 180, "Ringing", "B-early-f1");
    let mut other_caller = stack("C", "r9z8y7");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:other@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    let other = other_caller
        .out_of_dialog(Method::Invite, &addresses, &[], Vec::new(), None, None)
        .expect("another call's INVITE");
    assert_eq!(other.cseq().seq(), 1);
    uas.learn_request(&other);
    answer_invite(&mut uas, 487, "Request Terminated", "B-early-f9");
    assert!(uas.rings_early(), "the 487 to the other call ended this call's fork");
}
