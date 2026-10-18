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
