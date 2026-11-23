//! The live surfacing of the capability family rules.

use super::*;

/// §13.2.1 / §20.37 at the taker: ONE finding names both headers.
#[test]
fn a_re_invite_with_no_capabilities_is_reported_once_at_the_taker() {
    let evs = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(SUT, req("INVITE", "z9hG4bK-r", 2, Some("bt")), ALICE, 1),
    ];
    let out = live(RuleId::AllowSupportedOnInvite).check_positioned(&evs);
    assert_eq!(out.len(), 1, "one finding, both headers on it: {out:?}");
    assert_eq!(out[0].0, SUT, "reported on the lane that took the re-INVITE");
    assert!(
        out[0].1.contains("missing Allow:") && out[0].1.contains("missing Supported:"),
        "{}",
        out[0].1
    );
    assert_eq!(out[0].2, Some(2), "offending points at the re-INVITE");

    let advertised = vec![
        recv(SUT, req("INVITE", "z9hG4bK-i", 1, None), ALICE, 0),
        recv(
            SUT,
            in_dialog(
                "INVITE",
                B,
                "z9hG4bK-r",
                2,
                A,
                B,
                "Allow: INVITE, ACK\r\nSupported: 100rel\r\n",
            ),
            ALICE,
            1,
        ),
    ];
    assert!(live(RuleId::AllowSupportedOnInvite).check_positioned(&advertised).is_empty());
}

/// §8.2.1 at the taker: the lane that TOOK the unknown verb is the lane
/// charged, and the finding names what it answered instead.
#[test]
fn an_unrecognised_method_is_flagged_on_the_lane_that_took_it() {
    let clean = vec![
        recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
        sent(SUT, resp_with(405, 1, "FROBNICATE", "z9hG4bK-x", "Allow: INVITE, BYE\r\n"), ALICE, 1),
    ];
    assert!(live(RuleId::UnsupportedMethod405Allow).check_positioned(&clean).is_empty());

    let served = vec![
        recv(SUT, req_with("FROBNICATE", "z9hG4bK-x", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "FROBNICATE", "z9hG4bK-x", ""), ALICE, 1),
    ];
    let out = live(RuleId::UnsupportedMethod405Allow).check_positioned(&served);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT, "the lane that took the verb owed the 405");
    assert!(out[0].1.contains("405") && out[0].1.contains("got 200 / Allow=0"), "{}", out[0].1);
    assert_eq!(out[0].2, Some(1), "offending points at the request it answered wrong");
}

/// §8.2.2 at the taker: an unsupported `Require` served instead of refused,
/// with the tag named.
#[test]
fn an_unsupported_require_served_is_flagged() {
    let clean = vec![
        recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
        sent(
            SUT,
            resp_with(420, 1, "INVITE", "z9hG4bK-i", "Unsupported: frobnicate\r\n"),
            ALICE,
            1,
        ),
    ];
    assert!(live(RuleId::UnsupportedExtension420).check_positioned(&clean).is_empty());

    let served = vec![
        recv(SUT, req_with("INVITE", "z9hG4bK-i", "Require: frobnicate\r\n"), ALICE, 0),
        sent(SUT, resp_with(200, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 1),
    ];
    let out = live(RuleId::UnsupportedExtension420).check_positioned(&served);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("[frobnicate]") && out[0].1.contains("420"), "{}", out[0].1);
}

/// At the requester's lane, the UAS no recorded bind, a request owed a
/// rejection followed by `then`: the findings of `rule` the full audit
/// reports.
fn owed_rejection(
    rule: RuleId,
    request: Vec<u8>,
    then: Vec<Stamped<SignalingNetworkEvent>>,
) -> Vec<RfcFinding> {
    let mut evs = vec![sent(ALICE, request, SUT, 0)];
    evs.extend(then);
    evaluate_rfc_findings(&evs).into_iter().filter(|f| f.rule == rule.token()).collect()
}

/// What a lane's view misses when the rejection's only copy it was sent is
/// dropped on arrival, and what a drop on the call does not explain: the
/// rejection the loss model dropped toward the lane is excused, and with no
/// such drop the finding stands, charged to the unrecorded UAS.
fn a_lost_rejection_is_excused_and_no_other_drop_is(
    rule: RuleId,
    request: fn(&str) -> Vec<u8>,
    method: &str,
    rejection: (u16, &str),
) {
    use crate::types::RecvDisposition::LossModel;
    let (status, owed_header) = rejection;
    let branch = "z9hG4bK-n";
    let answer = |status: u16, cseq: u32| resp_with(status, cseq, method, branch, owed_header);

    let lost = vec![arrival(ALICE, answer(status, 1), SUT, 1, LossModel, None)];
    let excused = owed_rejection(rule, request(branch), lost);
    assert!(excused.is_empty(), "the {status} was sent and lost: {excused:?}");

    let other_method = resp_with(status, 1, "OPTIONS", branch, owed_header);
    let bare = resp_with(status, 1, method, branch, "");
    let unexplained = [
        ("no drop", vec![], "got 0"),
        (
            "a provisional lost",
            vec![arrival(ALICE, answer(100, 1), SUT, 1, LossModel, None)],
            "got 0",
        ),
        (
            "another final lost",
            vec![arrival(ALICE, answer(500, 1), SUT, 1, LossModel, None)],
            "got 0",
        ),
        (
            "another CSeq's final lost",
            vec![arrival(ALICE, answer(status, 2), SUT, 1, LossModel, None)],
            "got 0",
        ),
        (
            "another method's final lost",
            vec![arrival(ALICE, other_method, SUT, 1, LossModel, None)],
            "got 0",
        ),
        (
            "the owed status lost without its header",
            vec![arrival(ALICE, bare, SUT, 1, LossModel, None)],
            "got 0",
        ),
        (
            "a wrong final taken after the lost one",
            vec![
                arrival(ALICE, answer(status, 1), SUT, 1, LossModel, None),
                recv(ALICE, answer(200, 1), SUT, 2),
            ],
            "got 200",
        ),
    ];
    for (case, then, got) in unexplained {
        let found = owed_rejection(rule, request(branch), then);
        assert_eq!(found.len(), 1, "{case}: {found:?}");
        assert_eq!((found[0].lane.as_str(), found[0].charged.as_deref()), (ALICE, Some(SUT)));
        assert!(found[0].detail.contains(got), "{case}: {}", found[0].detail);
    }
}

/// §8.2.2 on a lossy wire: a 420 the loss model dropped on its way to the
/// requester was issued, whose re-sent copy the view does not hold.
#[test]
fn a_420_lost_toward_the_requester_is_excused_and_no_other_drop_is() {
    a_lost_rejection_is_excused_and_no_other_drop_is(
        RuleId::UnsupportedExtension420,
        |branch| req_with("INFO", branch, "Require: frobnicate\r\n"),
        "INFO",
        (420, "Unsupported: frobnicate\r\n"),
    );
}

/// §8.2.1 on a lossy wire: a 405 the loss model dropped on its way to the
/// requester was issued, whose re-sent copy the view does not hold.
#[test]
fn a_405_lost_toward_the_requester_is_excused_and_no_other_drop_is() {
    a_lost_rejection_is_excused_and_no_other_drop_is(
        RuleId::UnsupportedMethod405Allow,
        |branch| req_with("FROBNICATE", branch, ""),
        "FROBNICATE",
        (405, "Allow: INVITE, BYE\r\n"),
    );
}

/// The requester's copy of `request`, lost on its way to the UAS.
fn lost_on_send(request: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
    Stamped {
        event: SignalingNetworkEvent::SendLost {
            bind_key: ALICE.to_string(),
            to: SUT.parse().unwrap(),
            msg: request,
        },
        seq,
        at_ms: seq,
    }
}

/// §12.2.2 on a lossy wire, at the requester's lane: an in-dialog `method`
/// stating `extra`, owed a rejection, answered 481. The 481 is excused when
/// the request's copy was lost on send and the dialog's BYE 2xx came before
/// the 481: the UAS took a later copy on an ended dialog. With no lost copy,
/// or no BYE 2xx ahead of the 481, the 481 stays the miss.
fn a_481_after_the_bye_is_excused_only_behind_a_lost_copy(rule: RuleId, method: &str, extra: &str) {
    let request = || in_dialog(method, B, "z9hG4bK-n", 2, A, B, extra);
    let bye = || recv(ALICE, in_dialog("BYE", A, "z9hG4bK-b", 1, B, A, ""), SUT, 100);
    let bye_ok = || sent(ALICE, resp_with(200, 1, "BYE", "z9hG4bK-b", ""), SUT, 101);
    let refused = |seq| recv(ALICE, resp_with(481, 2, method, "z9hG4bK-n", ""), SUT, seq);

    let behind_a_lost_copy = vec![lost_on_send(request(), 1), bye(), bye_ok(), refused(600)];
    let excused = owed_rejection(rule, request(), behind_a_lost_copy);
    assert!(excused.is_empty(), "the 481 answered a later copy: {excused:?}");

    let unexplained = [
        ("no lost copy", vec![bye(), bye_ok(), refused(600)]),
        ("no BYE", vec![lost_on_send(request(), 1), refused(600)]),
        ("the BYE after the 481", vec![lost_on_send(request(), 1), refused(50), bye(), bye_ok()]),
    ];
    for (case, then) in unexplained {
        let found = owed_rejection(rule, request(), then);
        assert_eq!(found.len(), 1, "{case}: {found:?}");
        assert_eq!((found[0].lane.as_str(), found[0].charged.as_deref()), (ALICE, Some(SUT)));
        assert!(found[0].detail.contains("got 481"), "{case}: {}", found[0].detail);
    }
}

#[test]
fn a_481_to_an_unsupported_require_after_the_bye_is_excused_only_behind_a_lost_copy() {
    a_481_after_the_bye_is_excused_only_behind_a_lost_copy(
        RuleId::UnsupportedExtension420,
        "INFO",
        "Require: frobnicate\r\n",
    );
}

#[test]
fn a_481_to_an_unrecognised_method_after_the_bye_is_excused_only_behind_a_lost_copy() {
    a_481_after_the_bye_is_excused_only_behind_a_lost_copy(
        RuleId::UnsupportedMethod405Allow,
        "FROBNICATE",
        "",
    );
}

/// A request whose copy is lost on send still owes its rejection: the
/// requester retransmits it (RFC 3261 §17.1.2) and the UAS answers the copy
/// it takes. The lane's view holds no retransmitted copy, but a view kept
/// open until the transaction ends holds that answer, so its absence stays
/// the UAS's miss.
#[test]
fn a_request_lost_on_send_still_owes_its_rejection() {
    for (rule, request) in [
        (RuleId::UnsupportedExtension420, req_with("INFO", "z9hG4bK-n", "Require: frobnicate\r\n")),
        (RuleId::UnsupportedMethod405Allow, req_with("FROBNICATE", "z9hG4bK-n", "")),
    ] {
        let found = owed_rejection(rule, request.clone(), vec![lost_on_send(request, 1)]);
        assert_eq!(found.len(), 1, "{rule}: {found:?}");
        assert_eq!((found[0].lane.as_str(), found[0].charged.as_deref()), (ALICE, Some(SUT)));
        assert!(found[0].detail.contains("got 0"), "{rule}: {}", found[0].detail);
    }
}

/// §8.2.3 and §21.4.15 at the sender: the lane that SENT the bare final is
/// the lane charged.
#[test]
fn a_bare_415_and_a_bare_421_are_flagged_on_their_sender() {
    let bare_415 = vec![sent(SUT, resp_with(415, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
    let out = live(RuleId::Unsupported415Accepts).check_positioned(&bare_415);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Accept / Accept-Encoding / Accept-Language"), "{}", out[0].1);

    let with_accept = vec![sent(
        SUT,
        resp_with(415, 1, "INVITE", "z9hG4bK-i", "Accept: application/sdp\r\n"),
        ALICE,
        0,
    )];
    assert!(live(RuleId::Unsupported415Accepts).check_positioned(&with_accept).is_empty());

    let bare_421 = vec![sent(SUT, resp_with(421, 1, "INVITE", "z9hG4bK-i", ""), ALICE, 0)];
    let out = live(RuleId::UnsupportedExtension421).check_positioned(&bare_421);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].1.contains("Require"), "{}", out[0].1);

    let listed =
        vec![sent(SUT, resp_with(421, 1, "INVITE", "z9hG4bK-i", "Require: 100rel\r\n"), ALICE, 0)];
    assert!(live(RuleId::UnsupportedExtension421).check_positioned(&listed).is_empty());
}

/// §11.2 gates at the sender of the 2xx.
#[test]
fn a_bare_options_200_gates_at_its_sender() {
    assert!(!live(RuleId::OptionsResponseEchoes).force_advisory());
    let clean = vec![
        recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", "Allow: INVITE\r\n"), ALICE, 1),
    ];
    assert!(live(RuleId::OptionsResponseEchoes).check_positioned(&clean).is_empty());

    let bare = vec![
        recv(SUT, req_with("OPTIONS", "z9hG4bK-o", ""), ALICE, 0),
        sent(SUT, resp_with(200, 1, "OPTIONS", "z9hG4bK-o", ""), ALICE, 1),
    ];
    let out = live(RuleId::OptionsResponseEchoes).check_positioned(&bare);
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].0, SUT);
    assert!(out[0].1.contains("Allow/Supported/Accept"), "{}", out[0].1);
}

/// The strict reading refuses each of these requests, so the stack's 400 is
/// the answer they are owed: neither an unknown method, nor an unsupported
/// `Require`, nor both on a value-gated request, draws a 405 or a 420 demand.
#[test]
fn a_refused_request_answered_400_owes_no_other_rejection() {
    let with = |method: &str, branch: &str, extra: &str| {
        String::from_utf8(req(method, branch, 1, None))
            .expect("text")
            .replace("Content-Length:", &format!("{extra}Content-Length:"))
            .into_bytes()
    };
    let twice = "Subject: a\r\nSubject: b\r\n";
    let cases = [
        with("INVITE", "z9hG4bK-p1", &format!("Require: foo\r\n{twice}")),
        with("FOO", "z9hG4bK-q2", twice),
        with("OPTIONS", "z9hG4bK-q5", &format!("Require: foo\r\n{twice}")),
        with("INVITE", "z9hG4bK-q3", "Require: foo\r\nExpires: soon\r\n"),
    ];
    for refused in cases {
        let text = String::from_utf8_lossy(&refused).into_owned();
        let method = text.split(' ').next().unwrap_or_default().to_string();
        let branch = text.split("branch=").nth(1).and_then(|b| b.split("\r\n").next());
        let branch = branch.unwrap_or_default().to_string();
        let evs = vec![
            bind_roles(SUT, HashSet::from([UaRole::Uac, UaRole::Uas]), 0),
            recv(SUT, refused, ALICE, 1),
            sent(SUT, resp(400, 1, &method, "sut", &branch), ALICE, 2),
        ];
        let found: Vec<RfcFinding> = evaluate_rfc_findings(&evs)
            .into_iter()
            .filter(|f| {
                f.rule == RuleId::UnsupportedExtension420.token()
                    || f.rule == RuleId::UnsupportedMethod405Allow.token()
            })
            .collect();
        assert!(found.is_empty(), "{method} {branch}: {found:?}");
    }
}
