//! §15 against the establishing INVITE's final as the caller took it: a
//! non-2xx final with no 2xx before it ends the early dialog (§12.3), so a BYE
//! well after it names no dialog; a non-2xx after a 2xx is a stray that ends
//! nothing (§13.2.2.4, §17.1.1.2).

use crate::verdict::{Decision, Evidence, RuleId};

use super::super::{NoByeOutsideOrEarlyDialog, BYE_CROSSING_WINDOW_US};
use super::{eval, hits, in_dialog, invite, rsp, ALICE};

/// The caller BYE-ing an early dialog well after the non-2xx final that
/// ended it reached her (§12.3) names no dialog.
#[test]
fn a_caller_bye_long_after_the_final_names_no_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        rsp(3_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000 + 2 * BYE_CROSSING_WINDOW_US, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    let f = hits(&NoByeOutsideOrEarlyDialog, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&NoByeOutsideOrEarlyDialog, &msgs));
    assert_eq!(f[0].emitter, ALICE, "the caller is charged");
    let Decision::Violated(Evidence::ByeOffDialog { early_dialog, .. }) = &f[0].decision else {
        panic!("{:?}", f[0].decision)
    };
    assert!(!early_dialog, "the no-dialog shape");
}

/// A caller BYE seen shortly after the final may have left before the
/// final reached her: inside the window it settles nothing.
#[test]
fn a_caller_bye_inside_the_window_after_the_final_settles_nothing() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        rsp(3_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(103_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    let f = eval(&NoByeOutsideOrEarlyDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(matches!(f[0].decision, Decision::Undecidable(_)), "{:?}", f[0].decision);
}

/// A challenged INVITE retried and answered: the 2xx of the retry
/// confirms the dialog the challenge's tag also named, and the BYE on it
/// is the ordinary teardown.
#[test]
fn a_bye_after_a_challenge_retried_and_answered_is_compliant() {
    let mut retry = invite(4_000, "z9hG4bK-i2", "");
    retry.cseq = 2;
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 407, 1, "INVITE", "z9hG4bK-i", ""),
        retry,
        rsp(5_000, 200, 2, "INVITE", "z9hG4bK-i2", ""),
        in_dialog(5_000 + 2 * BYE_CROSSING_WINDOW_US, "BYE", "z9hG4bK-b", 3, "at", "bt"),
    ];
    assert!(
        hits(&NoByeOutsideOrEarlyDialog, &msgs).is_empty(),
        "{:?}",
        eval(&NoByeOutsideOrEarlyDialog, &msgs)
    );
}

/// A non-2xx final the caller takes on the establishing INVITE's branch
/// after its 2xx — a second final from another copy of the callee's side —
/// ends no dialog: her BYE long after it rides the confirmed dialog.
#[test]
fn a_stray_non_2xx_after_the_2xx_ends_no_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        rsp(3_000, 480, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000 + 2 * BYE_CROSSING_WINDOW_US, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    let f = eval(&NoByeOutsideOrEarlyDialog, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].rule, RuleId::NoByeOutsideOrEarlyDialog);
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}
