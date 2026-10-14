use crate::stack::tests::{calling, reliable, ringing, stack};
use crate::stack::*;

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

/// The caller's side of an INVITE its CANCEL ended: the callee answers the
/// CANCEL 200, then the INVITE 487, each under a To-tag and neither with a
/// Contact.
fn cancelled_before_any_ring() -> LegStack {
    let mut uac = calling();
    let (mut uas, _) = ringing("B");
    let answer = |status, reason, method| Answer {
        status,
        reason,
        cseq_method: Some(method),
        early_tag: None,
    };
    let trying = uas.respond(&answer(100, "Trying", "INVITE"), &[], Vec::new(), None).expect("100");
    uac.learn_response(&trying);
    let cancel = uac.cancel(&[]).expect("the caller CANCELs");
    uas.learn_request(&cancel);
    for (status, reason, method) in [(200, "OK", "CANCEL"), (487, "Request Terminated", "INVITE")] {
        let response = uas
            .respond(&answer(status, reason, method), &[], Vec::new(), None)
            .expect("the callee answers");
        assert!(response.contacts().as_slice().is_empty(), "{status} states no Contact");
        uac.learn_response(&response);
    }
    uac
}

/// RFC 3261 §12.1: only a 2xx or a provisional to a dialog-creating request
/// creates a dialog. The 200 to a CANCEL creates none, and neither does the
/// INVITE's 487, so the caller holds no dialog and an in-dialog request is
/// refused by name rather than composed with no remote target.
#[test]
fn a_cancelled_invite_leaves_the_caller_no_dialog() {
    let mut uac = cancelled_before_any_ring();
    assert!(!uac.has_dialog(), "the CANCEL's 200 opened a dialog");
    assert_eq!(
        uac.in_dialog(Method::Invite, &[], Vec::new(), None, None),
        Err(StackError::NoDialog { leg: "A".into(), method: "INVITE".into() })
    );
}

/// RFC 3261 §12.1: a 2xx to a request that creates no dialog opens none,
/// before an INVITE as after one. A caller whose OPTIONS drew a 200 still
/// holds no dialog.
#[test]
fn a_2xx_to_a_request_outside_a_dialog_opens_none() {
    let mut uac = stack("A", "r1a2b3");
    let addresses = Addresses {
        ruri: "sip:callee@pivot.invalid",
        from: "<sip:caller@pivot.invalid>",
        to: "<sip:callee@pivot.invalid>",
    };
    let options = uac
        .out_of_dialog(Method::Options, &addresses, &[], Vec::new(), None, None)
        .expect("the caller asks for capabilities");
    let mut uas = stack("B", "r1a2b3");
    uas.learn_request(&options);
    let ok = uas
        .respond(
            &Answer { status: 200, reason: "OK", cseq_method: Some("OPTIONS"), early_tag: None },
            &[],
            Vec::new(),
            None,
        )
        .expect("the callee answers");
    uac.learn_response(&ok);
    assert!(!uac.has_dialog(), "the OPTIONS' 200 opened a dialog");
}
