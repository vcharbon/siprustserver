//! Which final answers a §8.2 rejection occasion: a 481 of a dialog its
//! taker held ended (RFC 3261 §12.2.2, §15.1), and never a CANCEL's 200 (§9.2).

use super::*;

/// [`taken`] on the dialog `at`/`bt`.
fn taken_in_dialog(at_us: u64, method: &str, branch: &str, cseq: u32, extra: &str) -> Msg {
    Msg { to_tag: Some("bt".to_string()), ..taken(at_us, method, branch, cseq, extra) }
}

/// Alice's BYE of the dialog `at`/`bt`, answered 200 at `at_us`.
fn bye_ended(at_us: u64) -> [Msg; 2] {
    [
        taken_in_dialog(at_us - 500, "BYE", "z9hG4bK-b", 2, ""),
        answered(at_us, 200, "BYE", "z9hG4bK-b", 2, ""),
    ]
}

/// RFC 3261 §12.2.2: a UAS matches a To-tagged request to its dialogs before
/// it applies the §8.2 checks; one naming a dialog a BYE ended is refused
/// 481, which discharges the `Require` occasion.
#[test]
fn a_481_to_a_request_on_a_dialog_a_bye_ended_discharges_the_require() {
    let [bye, ok] = bye_ended(2_000);
    let msgs = [
        bye,
        ok,
        taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n"),
        answered(4_000, 481, "INFO", "z9hG4bK-i", 3, ""),
    ];
    let all = run(&UnsupportedExtension420, &msgs);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "{:?}", all[0].decision);
}

/// So for an unrecognised method: the 481 is the answer its ended dialog
/// owes, before §8.2.1 is reached.
#[test]
fn a_481_to_an_unknown_method_on_a_dialog_a_bye_ended_discharges_the_405() {
    let [bye, ok] = bye_ended(2_000);
    let msgs = [
        bye,
        ok,
        taken_in_dialog(3_000, "FROBNICATE", "z9hG4bK-x", 3, ""),
        answered(4_000, 481, "FROBNICATE", "z9hG4bK-x", 3, ""),
    ];
    assert!(violations(&UnsupportedMethod405Allow, &msgs).is_empty());
}

/// Bob's BYE of the dialog `at`/`bt` at `at_us`, and alice's 200 to it at
/// `ok_us`.
fn bye_from_bob(at_us: u64, ok_us: u64) -> [Msg; 2] {
    let bye = Msg {
        src: BOB.to_string(),
        dst: ALICE.to_string(),
        from_tag: Some("bt".to_string()),
        to_tag: Some("at".to_string()),
        ..taken(at_us, "BYE", "z9hG4bK-bb", 7, "")
    };
    let ok = Msg {
        src: ALICE.to_string(),
        dst: BOB.to_string(),
        from_tag: Some("bt".to_string()),
        to_tag: Some("at".to_string()),
        ..answered(ok_us, 200, "BYE", "z9hG4bK-bb", 7, "")
    };
    [bye, ok]
}

/// RFC 3261 §15.1.1 and RFC 5407 §3.3: a UA's dialog ends for new requests
/// once it sends or takes a BYE, so a request crossing bob's BYE draws the
/// 481 of a dialog bob no longer holds. Bob's vantage shows the crossing: his
/// BYE before the request. Alice's shows her request before bob's BYE, which
/// bob may have taken on the live dialog as well: that settles nothing.
#[test]
fn a_481_to_a_request_crossing_the_taker_s_bye_discharges_the_require() {
    let info = || taken_in_dialog(1_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = |at_us| answered(at_us, 481, "INFO", "z9hG4bK-i", 3, "");
    let [bye, ok] = bye_from_bob(1_200, 1_600);
    let at_bob = [bye.clone(), Msg { at_us: 1_300, ..info() }, refused(1_400), ok.clone()];
    let all = run(&UnsupportedExtension420, &at_bob);
    assert_eq!(all.len(), 1, "bob's: {all:?}");
    assert!(matches!(all[0].decision, Decision::Compliant), "bob's: {:?}", all[0].decision);

    let at_alice = [info(), bye, ok, refused(2_000)];
    let all = run(&UnsupportedExtension420, &at_alice);
    assert_eq!(all.len(), 1, "alice's: {all:?}");
    assert!(matches!(all[0].decision, Decision::Undecidable(_)), "alice's: {:?}", all[0].decision);
}

/// §8.2.2.3 and §15.1.2: a request its UAS took on a live dialog owes its
/// 420 whatever BYE follows it. Alice's own BYE behind her request reaches bob
/// after it, so his 481 is the miss; bob's BYE behind the request, alice's 200
/// to it, and then his 481, reads as the crossing it may be: undecided.
#[test]
fn a_481_to_a_request_taken_before_a_later_bye_still_owes_the_420() {
    let info = taken_in_dialog(1_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = |at_us| answered(at_us, 481, "INFO", "z9hG4bK-i", 3, "");
    let alice_s_bye = [
        info.clone(),
        taken_in_dialog(1_100, "BYE", "z9hG4bK-b", 4, ""),
        refused(1_300),
        answered(1_400, 200, "BYE", "z9hG4bK-b", 4, ""),
    ];
    let f = violations(&UnsupportedExtension420, &alice_s_bye);
    assert_eq!(f.len(), 1, "alice's BYE: {f:?}");
    let Decision::Violated(Evidence::RejectionNotIssued { answered_status, .. }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!(*answered_status, 481);

    let [bye, ok] = bye_from_bob(3_000, 3_500);
    let bob_s_bye = [info, bye, ok, refused(4_000)];
    let all = run(&UnsupportedExtension420, &bob_s_bye);
    assert_eq!(all.len(), 1, "bob's BYE: {all:?}");
    assert!(
        matches!(all[0].decision, Decision::Undecidable(_)),
        "bob's BYE: {:?}",
        all[0].decision
    );
}

/// So does a request bob takes after alice's BYE reached him, before he
/// answered that BYE.
#[test]
fn a_481_to_a_request_after_the_bye_reached_its_taker_discharges_the_require() {
    let [bye, _] = bye_ended(2_000);
    let msgs = [
        bye,
        taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n"),
        answered(3_500, 200, "BYE", "z9hG4bK-b", 2, ""),
        answered(4_000, 481, "INFO", "z9hG4bK-i", 3, ""),
    ];
    assert!(violations(&UnsupportedExtension420, &msgs).is_empty());
}

/// A vantage that carried the BYE's 2xx and not the BYE still shows the
/// dialog ended there; a BYE the strict reading refuses ends nothing.
#[test]
fn a_bye_s_2xx_ends_the_dialog_and_a_refused_bye_does_not() {
    let info = taken_in_dialog(3_000, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n");
    let refused = answered(4_000, 481, "INFO", "z9hG4bK-i", 3, "");
    let [bye, ok] = bye_ended(2_000);
    let only_the_2xx = [ok, info.clone(), refused.clone()];
    assert!(violations(&UnsupportedExtension420, &only_the_2xx).is_empty(), "the 2xx alone");
    let strictly_refused = Msg { refused: true, ..bye };
    let refused_bye = [strictly_refused, info, refused];
    assert_eq!(violations(&UnsupportedExtension420, &refused_bye).len(), 1, "a refused BYE");
}

/// A 481 on a dialog no BYE ended, to a request on no dialog (no To-tag),
/// or sent before any BYE of the dialog, still owes the 420.
#[test]
fn a_481_anywhere_else_is_violated() {
    let info = |at_us, to_tag: Option<&str>| Msg {
        to_tag: to_tag.map(str::to_string),
        ..taken(at_us, "INFO", "z9hG4bK-i", 3, "Require: frobnicate\r\n")
    };
    let refused = answered(4_000, 481, "INFO", "z9hG4bK-i", 3, "");
    let live = [info(3_000, Some("bt")), refused.clone()];
    assert_eq!(violations(&UnsupportedExtension420, &live).len(), 1, "a live dialog");
    let [bye, ok] = bye_ended(2_000);
    let out_of_dialog = [bye, ok, info(3_000, None), refused.clone()];
    assert_eq!(violations(&UnsupportedExtension420, &out_of_dialog).len(), 1, "no To-tag");
    let [late_bye, late_ok] = bye_from_bob(4_500, 5_000);
    let before_the_bye = [info(3_000, Some("bt")), refused, late_bye, late_ok];
    assert_eq!(violations(&UnsupportedExtension420, &before_the_bye).len(), 1, "before the BYE");
}

/// RFC 3261 §9.2: the 200 to a CANCEL answers the CANCEL's own transaction on
/// the INVITE's branch, never the INVITE. A re-INVITE bob rejects 420 after
/// accepting its CANCEL met §8.2.2.3; one he ends 487 instead did not, and the
/// finding names the 487.
#[test]
fn a_cancel_s_200_is_not_the_invite_s_answer() {
    let cancelled = |status, extra| {
        [
            taken_in_dialog(1_000, "INVITE", "z9hG4bK-r", 3, "Require: frobnicate\r\n"),
            taken_in_dialog(2_000, "CANCEL", "z9hG4bK-r", 3, ""),
            answered(3_000, 200, "CANCEL", "z9hG4bK-r", 3, ""),
            answered(4_000, status, "INVITE", "z9hG4bK-r", 3, extra),
        ]
    };
    let rejected = run(&UnsupportedExtension420, &cancelled(420, "Unsupported: frobnicate\r\n"));
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(matches!(rejected[0].decision, Decision::Compliant), "{:?}", rejected[0].decision);

    let ended = violations(&UnsupportedExtension420, &cancelled(487, ""));
    assert_eq!(ended.len(), 1, "{ended:?}");
    let Decision::Violated(Evidence::RejectionNotIssued { answered_status, .. }) =
        &ended[0].decision
    else {
        panic!("{:?}", ended[0].decision)
    };
    assert_eq!(*answered_status, 487, "the INVITE's own final, not the CANCEL's 200");
}
