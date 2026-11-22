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

    let lost_on_send = Stamped {
        event: SignalingNetworkEvent::SendLost {
            bind_key: ALICE.to_string(),
            to: SUT.parse().unwrap(),
            msg: request(branch),
        },
        seq: 1,
        at_ms: 1,
    };
    let unexplained = [
        ("no drop", vec![]),
        ("a provisional lost", vec![arrival(ALICE, answer(100, 1), SUT, 1, LossModel, None)]),
        ("another final lost", vec![arrival(ALICE, answer(500, 1), SUT, 1, LossModel, None)]),
        (
            "another CSeq's final lost",
            vec![arrival(ALICE, answer(status, 2), SUT, 1, LossModel, None)],
        ),
        ("the request lost on send", vec![lost_on_send]),
    ];
    for (case, then) in unexplained {
        let found = owed_rejection(rule, request(branch), then);
        assert_eq!(found.len(), 1, "{case}: {found:?}");
        assert_eq!((found[0].lane.as_str(), found[0].charged.as_deref()), (ALICE, Some(SUT)));
        assert!(found[0].detail.contains("got 0"), "{case}: {}", found[0].detail);
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
