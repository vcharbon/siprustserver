use crate::stack::tests::{calling, calling_with_cseq, header, reliable, ringing, stack, to_tag};
use crate::stack::*;

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

/// `response` as a peer that stated no Contact would have sent it.
fn without_contact(response: &SipResponse) -> SipResponse {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};
    let text = String::from_utf8(response.image().to_vec()).expect("text");
    let kept: Vec<&str> = text.split("\r\n").filter(|line| !line.starts_with("Contact:")).collect();
    match CustomParser::new().parse(kept.join("\r\n").as_bytes()).expect("parses") {
        SipMessage::Response(r) => r,
        _ => panic!("a response"),
    }
}

/// RFC 3261 §12.1.2: a dialog's remote target is the Contact its 2xx
/// carried. A 2xx that carried none leaves the dialog nothing to address,
/// so the ACK and the BYE on it are refused by name, never composed with
/// an empty Request-URI.
#[test]
fn a_confirmed_dialog_with_no_remote_target_refuses_its_requests_by_name() {
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
    let ok = without_contact(&ok);
    uac.learn_response(&ok);
    assert!(uac.has_dialog());
    assert_eq!(uac.remote_target(), "");
    assert_eq!(
        uac.ack_for(&ok, &[], Vec::new(), None, None),
        Err(StackError::NoRemoteTarget { leg: "A".into(), method: "ACK".into() })
    );
    assert_eq!(
        uac.in_dialog(Method::Bye, &[], Vec::new(), None, None),
        Err(StackError::NoRemoteTarget { leg: "A".into(), method: "BYE".into() })
    );
}

/// RFC 3261 §12.1.2 on an early dialog: its remote target is the Contact of
/// the provisional that opened it. A reliable provisional that carried none
/// leaves the PRACK and the UPDATE nothing to address, and both are refused
/// by name.
#[test]
fn an_early_dialog_with_no_remote_target_refuses_its_requests_by_name() {
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
    uac.learn_response(&without_contact(&rings));
    assert_eq!(
        uac.prack(None, &[], Vec::new(), None, None),
        Err(StackError::NoRemoteTarget { leg: "A".into(), method: "PRACK".into() })
    );
    assert_eq!(
        uac.update(None, &[], Vec::new(), None, None),
        Err(StackError::NoRemoteTarget { leg: "A".into(), method: "UPDATE".into() })
    );
}
