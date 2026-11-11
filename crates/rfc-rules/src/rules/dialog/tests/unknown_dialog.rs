//! UnknownDialog481 (RFC 3261 §12.2.2).

use super::*;

/// A BYE on the dialog the 200 confirmed is the obligation met.
#[test]
fn a_bye_on_a_confirmed_dialog_is_compliant() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
    ];
    let f = eval(&UnknownDialog481, &msgs);
    assert_eq!(f.len(), 1, "one occasion, at the endpoint that took it: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    assert_eq!(f[0].emitter, BOB, "the UAS that owed the 481 is charged");
    assert_eq!(f[0].taker, ALICE);
}

/// A request naming a peer tag the taker never confirmed is the §12.2.2
/// case, and serving it is the miss.
#[test]
fn a_request_for_an_unknown_peer_tag_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
        rsp(4_000, 200, 2, "BYE", "z9hG4bK-b", ""),
    ];
    let f = hits(&UnknownDialog481, &msgs);
    assert_eq!(f.len(), 1, "{f:?}");
    let Decision::Violated(Evidence::UnknownDialogRequest {
        method,
        from_tag,
        answered_status,
        ..
    }) = &f[0].decision
    else {
        panic!("{:?}", f[0].decision)
    };
    assert_eq!((method.as_str(), from_tag.as_str(), *answered_status), ("BYE", "zz", 200));
}

/// A request the strict reading refuses owes its 400, not the 481 its
/// unknown dialog would otherwise draw.
#[test]
fn a_refused_request_for_an_unknown_peer_tag_owes_no_481() {
    let mut refused = in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt");
    refused.refused = true;
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        refused,
        rsp(4_000, 400, 2, "BYE", "z9hG4bK-b", ""),
    ];
    assert!(eval(&UnknownDialog481, &msgs).is_empty());
}

/// The 481 discharges it.
#[test]
fn a_481_answers_the_unknown_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
        rsp(4_000, 481, 2, "BYE", "z9hG4bK-b", ""),
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A 481 the taker sends from another socket than the one the request
/// reached answers that request's transaction.
#[test]
fn a_481_from_a_second_socket_of_the_taker_answers_the_unknown_dialog() {
    let mut second = rsp(4_000, 481, 2, "BYE", "z9hG4bK-b", "");
    second.src = "127.0.0.1:41000".to_string();
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
        second,
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// Without a dialog-creating response on the call, nothing at this vantage
/// says which dialogs the taker has — an orphan request is no occasion.
#[test]
fn an_orphan_request_on_an_unseen_call_is_not_an_occasion() {
    let msgs = [in_dialog(1_000, "BYE", "z9hG4bK-orphan", 2, "at", "zz")];
    assert!(eval(&UnknownDialog481, &msgs).is_empty());
}

/// An ACK elicits no response (§17.1.1.3) and a CANCEL is matched by
/// transaction (§9.1): neither can be answered 481.
#[test]
fn ack_and_cancel_are_not_occasions() {
    for method in ["ACK", "CANCEL"] {
        let msgs = [
            invite(1_000, "z9hG4bK-i", ""),
            rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
            in_dialog(3_000, method, "z9hG4bK-x", 1, "zz", "bt"),
        ];
        assert!(eval(&UnknownDialog481, &msgs).is_empty(), "{method}");
    }
}

/// One relay face carries BOTH directions of a call, so it knows both of
/// its peers: the peer-tag key is what keeps it uncharged.
#[test]
fn a_face_that_carried_both_directions_knows_both_peers() {
    const PROXY: &str = "127.0.0.1:5080";
    let mut took_invite = invite(1_000, "z9hG4bK-i", "");
    took_invite.dst = PROXY.to_string();
    let mut passed_200 = rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "");
    passed_200.src = PROXY.to_string();
    let mut took_bye = in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
    took_bye.dst = PROXY.to_string();
    let msgs = [took_invite, passed_200, took_bye];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// An unanswered request inside an OPEN observation's window is truncation,
/// not silence.
#[test]
fn an_unanswered_request_inside_the_window_settles_nothing() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
    ];
    let open = Observation { last_us: 3_500, closed: false, ..Observation::default() };
    let f = UnknownDialog481.eval(&WireView { msgs: &msgs, obs: &open });
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(!f[0].decided(), "{:?}", f[0].decision);
    assert_eq!(hits(&UnknownDialog481, &msgs).len(), 1, "closed, it decides at once");
}

/// §12.3: the non-2xx final to the establishing INVITE ends the early
/// dialog its tagged provisional opened, so a BYE the UAS takes after
/// sending that final names no dialog and serving it is the miss.
#[test]
fn a_bye_after_the_final_that_ended_its_early_dialog_is_violated() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        rsp(3_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(4_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        rsp(5_000, 200, 2, "BYE", "z9hG4bK-b", ""),
    ];
    let f = hits(&UnknownDialog481, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&UnknownDialog481, &msgs));
    assert_eq!(f[0].emitter, BOB, "the UAS that ended the dialog owed the 481");

    let mut refused = msgs.clone();
    refused[4] = rsp(5_000, 481, 2, "BYE", "z9hG4bK-b", "");
    assert!(
        hits(&UnknownDialog481, &refused).is_empty(),
        "{:?}",
        eval(&UnknownDialog481, &refused)
    );
}

/// A tagged non-2xx final opens no dialog: a BYE after it is the miss too.
#[test]
fn a_tagged_rejection_opens_no_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 486, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        rsp(4_000, 200, 2, "BYE", "z9hG4bK-b", ""),
    ];
    assert_eq!(hits(&UnknownDialog481, &msgs).len(), 1, "{:?}", eval(&UnknownDialog481, &msgs));
}

/// The early dialog still exists while the INVITE is pending: a BYE taken
/// before the final is the caller's legitimate early BYE (§15), whatever
/// final follows.
#[test]
fn a_bye_taken_before_the_final_rides_the_early_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        rsp(4_000, 200, 2, "BYE", "z9hG4bK-b", ""),
        rsp(5_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A view that opens mid-call carries no establishing INVITE: a re-INVITE
/// it rejects (503, 491) ends nothing, and the BYE after it rides the
/// dialog the earlier tagged answer showed.
#[test]
fn a_rejected_reinvite_mid_call_ends_nothing() {
    for status in [503, 491] {
        let msgs = [
            in_dialog(1_000, "INFO", "z9hG4bK-n", 2, "at", "bt"),
            rsp(2_000, 200, 2, "INFO", "z9hG4bK-n", ""),
            in_dialog(3_000, "INVITE", "z9hG4bK-r", 3, "at", "bt"),
            rsp(4_000, status, 3, "INVITE", "z9hG4bK-r", ""),
            in_dialog(5_000, "BYE", "z9hG4bK-b", 4, "at", "bt"),
            rsp(6_000, 200, 4, "BYE", "z9hG4bK-b", ""),
        ];
        assert!(
            hits(&UnknownDialog481, &msgs).is_empty(),
            "{status}: {:?}",
            eval(&UnknownDialog481, &msgs)
        );
    }
}

/// A tagged rejection of a non-INVITE request shows the dialog it was
/// sent in: the BYE after an UPDATE answered 405 rides that dialog.
#[test]
fn a_rejected_update_shows_its_dialog() {
    let msgs = [
        in_dialog(1_000, "UPDATE", "z9hG4bK-u", 2, "at", "bt"),
        rsp(2_000, 405, 2, "UPDATE", "z9hG4bK-u", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 3, "at", "bt"),
        rsp(4_000, 200, 3, "BYE", "z9hG4bK-b", ""),
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A captured flow that starts mid-call: its first tagged answer is a
/// re-INVITE's 503, which shows the dialog rather than ending it.
#[test]
fn a_flow_starting_mid_call_is_not_charged() {
    let msgs = [
        in_dialog(1_000, "INVITE", "z9hG4bK-r", 3, "at", "bt"),
        rsp(2_000, 503, 3, "INVITE", "z9hG4bK-r", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 4, "at", "bt"),
        rsp(4_000, 200, 4, "BYE", "z9hG4bK-b", ""),
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A request is judged once per transaction, at its first copy: a BYE
/// taken before the 487 is the caller's early BYE, and its retransmission
/// after the 487 is the same act again.
#[test]
fn a_bye_retransmitted_across_the_final_is_judged_once() {
    let mut again = in_dialog(5_000, "BYE", "z9hG4bK-b", 2, "at", "bt");
    again.repeat = true;
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "at", "bt"),
        rsp(4_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
        again,
        rsp(6_000, 200, 2, "BYE", "z9hG4bK-b", ""),
    ];
    let f = eval(&UnknownDialog481, &msgs);
    assert_eq!(f.len(), 1, "one occasion for one BYE transaction: {f:?}");
    assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
}

/// A PRACK on the early dialog a final ended is judged by the `RSeq` it
/// names (RFC 3262 §3, `Prack2xxOr481`), not here.
#[test]
fn a_prack_after_the_final_is_left_to_the_rseq_rule() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 180, 1, "INVITE", "z9hG4bK-i", "RSeq: 1\r\nRequire: 100rel\r\n"),
        rsp(3_000, 487, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(4_000, "PRACK", "z9hG4bK-p", 2, "at", "bt"),
        rsp(5_000, 200, 2, "PRACK", "z9hG4bK-p", ""),
    ];
    assert!(eval(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A non-2xx final to a re-INVITE leaves the confirmed dialog in place
/// (§14.1): only the establishing exchange's failure ends a dialog here.
#[test]
fn a_rejected_reinvite_keeps_the_confirmed_dialog() {
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", ""),
        in_dialog(3_000, "INVITE", "z9hG4bK-r", 2, "at", "bt"),
        rsp(4_000, 488, 2, "INVITE", "z9hG4bK-r", ""),
        in_dialog(5_000, "BYE", "z9hG4bK-b", 3, "at", "bt"),
        rsp(6_000, 200, 3, "BYE", "z9hG4bK-b", ""),
    ];
    assert!(hits(&UnknownDialog481, &msgs).is_empty(), "{:?}", eval(&UnknownDialog481, &msgs));
}

/// A UAS that answers from another socket than the one it takes requests at
/// knows the dialogs its answers created at both: a request it takes at the
/// first naming an unknown peer tag owes the 481 there.
#[test]
fn a_two_socket_uas_owes_the_481_where_it_takes_the_request() {
    const BOB_SENDS: &str = "127.0.0.1:5071";
    let from_second = |mut m: Msg| {
        m.src = BOB_SENDS.to_string();
        m
    };
    let msgs = [
        invite(1_000, "z9hG4bK-i", ""),
        from_second(rsp(2_000, 200, 1, "INVITE", "z9hG4bK-i", "")),
        in_dialog(3_000, "BYE", "z9hG4bK-b", 2, "zz", "bt"),
        from_second(rsp(4_000, 200, 2, "BYE", "z9hG4bK-b", "")),
    ];
    let f = hits(&UnknownDialog481, &msgs);
    assert_eq!(f.len(), 1, "{:?}", eval(&UnknownDialog481, &msgs));
    assert_eq!(f[0].emitter, BOB, "the socket that took the request is charged");
}
