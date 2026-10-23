//! Unit tests locking the pure lens/accessor/timer helpers. No direct TS
//! counterpart (the source exercised these via the deferred CallState tests);
//! analogous to slice-1's message-layer smoke tests.

use crate::common;
use call::helpers::*;
use call::model::*;
use common::representative_call;
use sip_retransmit::Class;

const A_TAG: &str = "b2bua-to-tag-aleg-9876"; // the a-leg dialog's own tag
const ALICE_TAG: &str = "alice-from-tag-001"; // the a-leg dialog's peer tag
const B_LOCAL_TAG: &str = "b2bua-from-tag-bleg-5544"; // the b-1 dialog's own tag
const B_TAG: &str = "bob-to-tag-007"; // the b-1 dialog's peer tag

fn a_id() -> DialogId {
    DialogId { our_tag: A_TAG.into(), peer_tag: ALICE_TAG.into() }
}

fn b_id() -> DialogId {
    DialogId { our_tag: B_LOCAL_TAG.into(), peer_tag: B_TAG.into() }
}

fn a_dialog(call: &Call) -> &Dialog {
    call.a_leg.dialogs.by_tags(A_TAG, ALICE_TAG).expect("the a-leg dialog")
}

fn b_dialog(call: &Call) -> &Dialog {
    call.b_legs[0].dialogs.by_tags(B_LOCAL_TAG, B_TAG).expect("the b-1 dialog")
}

#[test]
fn find_and_kind_accessors() {
    let call = representative_call();
    assert_eq!(find_leg(&call, "a").unwrap().leg_id, "a");
    assert_eq!(find_b_leg(&call, "b-1").unwrap().leg_id, "b-1");
    assert!(find_b_leg(&call, "nope").is_none());
    assert_eq!(find_b_leg_by_call_id(&call, "b-leg-call-id-fedcba@b2bua").unwrap().leg_id, "b-1");
    assert_eq!(leg_kind(&call.a_leg), LegKind::A);
    assert!(is_adopted(&call.a_leg));
}

#[test]
fn leg_kind_and_adoption_defaults() {
    let mut leg = representative_call().a_leg;
    leg.leg_id = "b-2".into();
    leg.kind = None;
    assert_eq!(leg_kind(&leg), LegKind::Destination);
    assert!(is_adopted(&leg));
    leg.kind = Some(LegKind::Media);
    assert!(!is_adopted(&leg));
    leg.adopted = Some(true); // explicit flag wins
    assert!(is_adopted(&leg));
}

#[test]
fn cseq_lens_helpers() {
    let call = representative_call();
    let before = a_dialog(&call).our_cseq;
    let call = bump_our_cseq(call, "a", &a_id(), 5);
    assert_eq!(a_dialog(&call).our_cseq, before + 5);

    let call = advance_peer_cseq(call, "b-1", &b_id(), 9999);
    assert_eq!(b_dialog(&call).peer_cseq, Some(9999));
}

/// §12.2.2: a request in order sets the dialog's remote sequence number, which
/// never decreases.
#[test]
fn the_peer_sequence_never_decreases() {
    let mut unseen = representative_call();
    unseen.b_legs[0].dialogs.by_tags_mut(B_LOCAL_TAG, B_TAG).unwrap().peer_cseq = None;
    let call = advance_peer_cseq(unseen, "b-1", &b_id(), 7);
    assert_eq!(b_dialog(&call).peer_cseq, Some(7), "the first request sets it");
    let call = advance_peer_cseq(call, "b-1", &b_id(), 12);
    assert_eq!(b_dialog(&call).peer_cseq, Some(12), "a gap is in order");
    let call = advance_peer_cseq(call, "b-1", &b_id(), 9);
    assert_eq!(b_dialog(&call).peer_cseq, Some(12), "a lower number leaves it");
}

#[test]
fn pending_request_lifecycle() {
    let call = representative_call();
    let entry = PendingRequest {
        source_call_id: "cid".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        ..PendingRequest::new("OPTIONS", 42, 42, Direction::FromB)
    };
    let call = add_pending_request(call, "b-1", &b_id(), entry);
    assert!(find_pending_request(b_dialog(&call), 42).is_some());

    let call = remove_pending_request(call, "b-1", &b_id(), 42);
    assert!(find_pending_request(b_dialog(&call), 42).is_none());
}

#[test]
fn tag_accessors_and_fronts() {
    let call = representative_call();
    assert_eq!(leg_own_tag(&call.a_leg), Some(A_TAG));
    assert_eq!(caller_dialog(&call, A_TAG).unwrap().peer_tag, ALICE_TAG);
    assert_eq!(leg_own_tag(&call.b_legs[0]), Some(B_LOCAL_TAG));
    assert_eq!(callee_dialog(&call.b_legs[0], B_TAG).unwrap().our_tag, B_LOCAL_TAG);

    // The caller-facing dialog fronts the callee dialog, found from either side.
    let fronting = fronting_dialog(&call, "b-1", B_TAG).expect("a fronting dialog");
    assert_eq!(fronting.our_tag, A_TAG);
    assert_eq!(fronting.fronts, vec![PeerDialog { leg_id: "b-1".into(), peer_tag: B_TAG.into() }]);
    assert!(fronting_dialog(&call, "b-1", "another-fork").is_none());

    // Inserting a dialog under tags a record already holds replaces it.
    let mut dialogs = call.a_leg.dialogs.clone();
    let n_before = dialogs.len();
    dialogs.insert(Dialog::new(A_TAG, ALICE_TAG, DialogState::Early, 1));
    assert_eq!(dialogs.len(), n_before);
}

#[test]
fn peering_split_merge() {
    let call = representative_call();
    assert_eq!(get_peer(&call, "a"), Some("b-1"));
    assert_eq!(get_peer(&call, "b-1"), Some("a"));
    assert_eq!(all_peered_legs(&call).len(), 2);

    let call = split_leg(call, "a");
    assert_eq!(get_peer(&call, "a"), None);
    assert!(all_peered_legs(&call).is_empty());

    let call = merge_leg(call, "a", "b-1");
    assert_eq!(get_peer(&call, "b-1"), Some("a"));
}

fn end_dialog(call: Call, leg_id: &str, state: DialogState) -> Call {
    update_leg(call, leg_id, |l| {
        for d in l.dialogs.iter_mut() {
            d.state = state.clone();
        }
    })
}

#[test]
fn termination_resolution() {
    let call = representative_call();
    // Confirmed legs no BYE ends are not yet resolved.
    assert!(!is_fully_resolved(&call));

    let call = end_dialog(call, "a", DialogState::Ending(EndingBy::Bye));
    let call = end_dialog(call, "b-1", DialogState::Ended(DialogEnd::PeerBye));
    assert!(!is_fully_resolved(&call), "a BYE in flight holds its leg");
    let call =
        end_dialog(call, "a", DialogState::Ended(DialogEnd::OurBye { final_status: Some(200) }));
    assert!(is_fully_resolved(&call));
}

/// A leg's state, end and disposition read off its INVITE facts and its
/// dialogs' states (RFC 3261 §9.1, §12, §15).
#[test]
fn a_legs_state_reads_off_its_invite_and_its_dialogs() {
    let mut leg = representative_call().b_legs[0].clone();
    leg.dialogs.retain(|_| false);
    let invite = leg.outbound_invite.clone().expect("a dialled leg");
    let read = |l: &Leg| (l.state(), l.end(), l.disposition(), l.is_resolved());
    assert_eq!(read(&leg), (LegState::Trying, None, LegDisposition::Pending, true));

    leg.dialogs.insert(Dialog::new(&invite.our_tag, "fork", DialogState::Early, 1));
    assert_eq!(read(&leg), (LegState::Early, None, LegDisposition::Pending, false));

    let mut cancelling = leg.clone();
    cancelling.outbound_invite.as_mut().unwrap().cancel_sent = true;
    for d in cancelling.dialogs.iter_mut() {
        d.state = DialogState::Ending(EndingBy::Cancel);
    }
    let cancelled = Some(ByeDisposition::Cancelled);
    assert_eq!(
        read(&cancelling),
        (LegState::Terminated, cancelled, LegDisposition::Cancelling, false)
    );
    cancelling.outbound_invite.as_mut().unwrap().record_final(487);
    assert_eq!(
        read(&cancelling),
        (LegState::Terminated, cancelled, LegDisposition::Rejected, true)
    );

    let mut rejected = leg.clone();
    rejected.outbound_invite.as_mut().unwrap().record_final(486);
    let end = Some(ByeDisposition::Rejected);
    assert_eq!(read(&rejected), (LegState::Terminated, end, LegDisposition::Rejected, true));

    leg.outbound_invite.as_mut().unwrap().record_final(200);
    leg.dialogs.iter_mut().for_each(|d| d.state = DialogState::Confirmed);
    assert_eq!(read(&leg), (LegState::Confirmed, None, LegDisposition::Bridged, false));
    leg.dialogs.iter_mut().for_each(|d| d.state = DialogState::Ending(EndingBy::Bye));
    let sent = Some(ByeDisposition::ByeSent);
    assert_eq!(read(&leg), (LegState::Terminated, sent, LegDisposition::Bridged, false));
    let done = DialogState::Ended(DialogEnd::OurBye { final_status: None });
    leg.dialogs.iter_mut().for_each(|d| d.state = done.clone());
    let timed_out = Some(ByeDisposition::ByeTimeout);
    assert_eq!(read(&leg), (LegState::Terminated, timed_out, LegDisposition::Bridged, true));
}

/// The caller's leg: her INVITE's final is the one this stack sent; an
/// unanswered caller holds the call for nothing (the call answers her as it
/// ends).
#[test]
fn the_callers_leg_reads_the_final_it_was_sent() {
    let mut leg = representative_call().a_leg;
    let read = |l: &Leg| (l.state(), l.end(), l.disposition(), l.is_resolved());
    leg.invite_final_sent = None;
    leg.dialogs.iter_mut().for_each(|d| d.state = DialogState::Early);
    assert_eq!(read(&leg), (LegState::Early, None, LegDisposition::Pending, true));
    let mut rejected = leg.clone();
    rejected.invite_final_sent = Some(480);
    rejected
        .dialogs
        .iter_mut()
        .for_each(|d| d.state = DialogState::Ended(DialogEnd::Rejected(480)));
    let end = Some(ByeDisposition::Rejected);
    assert_eq!(read(&rejected), (LegState::Terminated, end, LegDisposition::Rejected, true));
    leg.invite_final_sent = Some(200);
    leg.dialogs.iter_mut().for_each(|d| d.state = DialogState::Confirmed);
    assert_eq!(read(&leg), (LegState::Confirmed, None, LegDisposition::Bridged, false));
}

#[test]
fn ext_and_rule_helpers() {
    let call = representative_call();
    let call = set_call_ext(call, "transfer", Some(serde_json::json!({ "phase": "ringing" })));
    assert!(call.ext.as_ref().unwrap().contains_key("transfer"));
    let call = set_call_ext(call, "transfer", None);
    assert!(!call.ext.as_ref().unwrap().contains_key("transfer"));

    let call = set_leg_ext(call, "b-1", "media", serde_json::json!({ "role": "mrf" }));
    assert!(call.b_legs[0].ext.as_ref().unwrap().contains_key("media"));

    let call = deactivate_rule(call, "limit-by-subscriber");
    let r =
        call.active_rules.as_ref().unwrap().iter().find(|r| r.id == "limit-by-subscriber").unwrap();
    assert!(!r.active);
}

#[test]
fn dialog_constructor() {
    let empty = Dialog::new("lt", "rt", DialogState::Early, 1000);
    assert_eq!((empty.our_tag.as_str(), empty.peer_tag.as_str()), ("lt", "rt"));
    assert_eq!(empty.our_cseq, 1000);
    assert_eq!(empty.remote_target, "");
    assert_eq!(empty.peer_cseq, None);
    assert!(empty.route_set.is_empty());
    assert!(empty.fronts.is_empty());
    assert!(empty.inbound_pending_requests.is_empty());
    assert!(empty.is_live() && !empty.is_confirmed());
}

#[test]
fn arming_a_key_replaces_its_instance_under_the_next_generation() {
    let mut call = representative_call();
    let n = call.timers.len();
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "a");
    let rearmed = arm_timer(&mut call, no_answer.clone(), 9_999);
    assert_eq!(call.timers.len(), n, "replaced, not appended");
    assert_eq!(rearmed.generation, 3, "the call's next generation");
    assert!(holds_timer(&call, &no_answer, 3));
    assert!(!holds_timer(&call, &no_answer, 1), "the replaced instance is gone");

    // A brand-new key appends.
    let keepalive = arm_timer(&mut call, TimerKey::call(TimerType::Keepalive), 1);
    assert_eq!(call.timers.len(), n + 1);
    assert_eq!(keepalive.generation, 4);
    assert_eq!(disarm_timer(&mut call, &keepalive.key), Some(keepalive));
    assert_eq!(call.timers.len(), n);
}

#[test]
fn dump_cursors_renders_sorted_or_dash() {
    // No active machine → a dash, not an empty string.
    let mut call = representative_call();
    call.sm_cursors.clear();
    assert_eq!(dump_cursors(&call), "-");

    // Multiple machines render in MachineId order, `machine=state` joined by space.
    call.sm_cursors.insert(MachineId::new("transfer"), StateLabel::new("CRinging"));
    call.sm_cursors.insert(MachineId::new("global-call"), StateLabel::new("Active"));
    assert_eq!(dump_cursors(&call), "global-call=Active transfer=CRinging");
}

// ── the reliable provisionals a call received (RFC 3262) ────────────────────

/// The responder's identity of a reliable provisional on `leg`'s dialog `tag`.
fn pid(leg: &str, tag: &str, cseq: u32, rseq: u32) -> ProvisionalId {
    ProvisionalId {
        leg: leg.into(),
        responder_tag: tag.into(),
        invite_cseq: CSeq(cseq),
        rseq: RSeq(rseq),
    }
}

/// Record `id` as it arrives, then show it in this stack's `tag` dialog under
/// the INVITE `shown_cseq` there, the dialog's first number being `initial`.
fn shown(
    mut call: Call,
    id: &ProvisionalId,
    tag: &str,
    shown_cseq: u32,
    initial: u32,
) -> (Call, RSeq) {
    record_provisional(&mut call, id.clone(), false);
    let rseq = show_provisional(&mut call, id, tag, CSeq(shown_cseq), false, RSeq(initial))
        .expect("a recorded provisional is shown");
    (call, rseq)
}

#[test]
fn a_fresh_reliable_provisional_takes_the_next_number() {
    let (call, first) = shown(representative_call(), &pid("b-1", "bf1", 1, 4711), "a1", 1, 9_000);
    assert_eq!(first, RSeq(9_000), "the first shown provisional takes the random start");
    let (call, second) = shown(call, &pid("b-1", "bf1", 1, 4712), "a1", 1, 9_000);
    assert_eq!(
        second,
        RSeq(9_001),
        "RFC 3262 §4: the next in this dialog is greater by exactly one"
    );
    assert_eq!(call.provisionals.len(), 2);
    assert_eq!(responder_of(&call, "a1", RSeq(9_001)), Some(&pid("b-1", "bf1", 1, 4712)));
}

#[test]
fn showing_the_same_provisional_again_is_the_same_number() {
    let id = pid("b-1", "bf1", 1, 4711);
    let (mut call, first) = shown(representative_call(), &id, "a1", 1, 9_000);
    let again = show_provisional(&mut call, &id, "a1", CSeq(1), false, RSeq(1));
    assert_eq!(again, Some(first), "a recorded number is recalled");
    assert_eq!(call.provisionals.len(), 1);
}

/// RFC 3262 §3/§4: a copy of a recorded provisional is a retransmission, the
/// next of its dialog's sequence is in order, and anything else is out of
/// order — per responder dialog and INVITE.
#[test]
fn an_arrival_reads_as_a_repeat_in_order_or_out_of_order() {
    let mut call = settled_call();
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 1, 4711)), Arrival::InOrder, "a dialog's first");
    record_provisional(&mut call, pid("b-1", "bf1", 1, 4711), false);
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 1, 4711)), Arrival::Repeat);
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 1, 4712)), Arrival::InOrder);
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 1, 4713)), Arrival::OutOfOrder, "a gap");
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 1, 4710)), Arrival::OutOfOrder, "below");
    assert_eq!(arrival(&call, &pid("b-1", "bf2", 1, 9)), Arrival::InOrder, "another fork");
    assert_eq!(arrival(&call, &pid("b-1", "bf1", 2, 4711)), Arrival::InOrder, "a later INVITE");
}

/// `representative_call` deliberately seeds pending INVITE snapshots for
/// round-trip coverage; these start from a call with no in-dialog INVITE in
/// flight.
fn settled_call() -> Call {
    let mut call = representative_call();
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        for d in leg.dialogs.iter_mut() {
            d.inbound_pending_requests.clear();
        }
    }
    call
}

/// The `RAck` tokens a well-formed PRACK carries for `rseq` under `cseq`.
fn rack(rseq: RSeq, cseq: u32) -> RAckTokens {
    RAckTokens { rseq, cseq: CSeq(cseq), names_invite: true }
}

/// RFC 3262 §3: a PRACK naming no provisional this face showed draws 481 here,
/// and the face's sequence is this stack's own — what the responder numbered
/// says nothing about the RAck.
#[test]
fn an_rseq_this_dialog_never_showed_draws_481() {
    let (call, shown_as) =
        shown(settled_call(), &pid("b-1", "bf1", 1, 13_213_449), "a1", 1, 625_707);
    assert_eq!(
        prack_disposition(&call, "a1", rack(shown_as, 1), None),
        PrackDisposition::Answer200
    );
    assert_eq!(
        prack_disposition(&call, "a1", rack(RSeq(13_213_449), 1), None),
        PrackDisposition::Refuse481,
        "the responder's number means nothing on this face",
    );
}

/// RFC 3262 §7.2: the `RAck` is three tokens, and a PRACK matches only where
/// all three name the provisional.
#[test]
fn a_shown_rseq_under_the_wrong_cseq_or_method_names_nothing() {
    let (call, s) = shown(settled_call(), &pid("b-1", "bf1", 7, 4711), "a1", 1, 9_000);
    assert!(named_by(&call, "a1", rack(s, 1)).is_some());
    assert!(named_by(&call, "a1", rack(s, 101)).is_none(), "the CSeq token is a stranger");
    assert!(named_by(&call, "a1", rack(s, 7)).is_none(), "the responder's CSeq is not this face's");
    assert!(
        named_by(&call, "a1", RAckTokens { rseq: s, cseq: CSeq(1), names_invite: false }).is_none(),
        "no reliable provisional answers anything but an INVITE"
    );
    assert!(named_by(&call, "a2", rack(s, 1)).is_none(), "another shown dialog's number");
}

/// The disposition of a PRACK (RFC 3262 §3): relayed while its responder is
/// owed a PRACK on a dialog that carries one, answered 200 once this stack
/// PRACKed it itself or its dialog is gone, refused 481 once its party
/// acknowledged it — except for the very PRACK that acknowledged it.
#[test]
fn a_prack_is_relayed_answered_or_refused_by_what_it_names() {
    let id = pid("b-1", B_TAG, 1, 4711);
    let (mut call, s) = shown(settled_call(), &id, "a1", 1, 9_000);
    assert_eq!(prack_disposition(&call, "a1", rack(s, 1), None), PrackDisposition::Relay);
    assert!(acknowledge_shown(&mut call, "a1", s));
    let this_one = Obligation::AwaitPrack { tag: "a1".into(), rseq: s };
    assert_eq!(
        prack_disposition(&call, "a1", rack(s, 1), Some(&this_one)),
        PrackDisposition::Relay,
        "the PRACK that acknowledged it is answered for it",
    );
    assert_eq!(
        prack_disposition(&call, "a1", rack(s, 1), None),
        PrackDisposition::Refuse481,
        "a later PRACK names nothing unacknowledged",
    );

    let other = pid("b-1", B_TAG, 1, 4712);
    let (mut call, s2) = shown(call, &other, "a1", 1, 9_000);
    note_responder_ack(&mut call, &other, ResponderAck::OwnPrack { branch: "z9hG4bK-own".into() });
    assert_eq!(prack_disposition(&call, "a1", rack(s2, 1), None), PrackDisposition::Answer200);
    assert!(own_prack_branch(&call, "z9hG4bK-own"));
    assert!(!own_prack_branch(&call, "z9hG4bK-relayed"));
}

/// A responder dialog that no longer carries a request — gone, or its INVITE
/// being cancelled — leaves a still-owed provisional answered 200 here.
#[test]
fn a_responder_dialog_that_carries_nothing_answers_200_here() {
    let id = pid("b-1", "gone-fork", 1, 4711);
    let (call, s) = shown(settled_call(), &id, "a1", 1, 9_000);
    assert_eq!(prack_disposition(&call, "a1", rack(s, 1), None), PrackDisposition::Answer200);
}

/// RFC 3262 §3 admits a reliable provisional toward a relayed request's
/// originator only for an INVITE, and only one whose originator offered
/// `100rel` itself; the request this stack built toward the target may offer
/// it where the originator did not. A snapshot hydrated without the offer
/// recorded admits nothing.
#[test]
fn a_relayed_provisional_is_reliable_only_to_an_invite_whose_originator_offered_100rel() {
    let pending = |method: &str, offered_100rel: bool| PendingRequest {
        offered_100rel,
        ..PendingRequest::new(method, 2, 2, Direction::FromA)
    };
    assert!(admits_reliable_provisional(&pending("INVITE", true)));
    assert!(
        !admits_reliable_provisional(&pending("INVITE", false)),
        "no offer, no reliable provisional"
    );
    assert!(
        !admits_reliable_provisional(&pending("UPDATE", true)),
        "the mechanism serves INVITE alone"
    );
    assert!(!admits_reliable_provisional(&pending("OPTIONS", false)));
}

/// The leg a number was shown on is the one whose dialog carries the shown
/// tag as this stack's own; a tag no dialog carries names no leg.
#[test]
fn the_shown_leg_is_the_one_whose_dialog_carries_the_shown_tag() {
    let call = settled_call();
    assert_eq!(leg_shown(&call, "b2bua-to-tag-aleg-9876"), Some("a"));
    assert_eq!(leg_shown(&call, "b2bua-from-tag-bleg-5544"), Some("b-1"));
    assert_eq!(leg_shown(&call, "alice-from-tag-001"), None, "the peer's tag is not a shown one");
    assert_eq!(leg_shown(&call, "fork-2-a-tag"), None);
}

/// Forks mirrored as DISTINCT shown dialogs each carry their own sequence
/// (RFC 3262 §4, errata 4603/4604), so interleaving them never shows either
/// dialog a gap.
#[test]
fn each_shown_dialog_carries_its_own_sequence() {
    let (call, one) = shown(representative_call(), &pid("b-1", "bf1", 1, 1), "a1", 1, 5);
    let (call, two) = shown(call, &pid("b-1", "bf2", 1, 200), "a2", 1, 40);
    let (call, three) = shown(call, &pid("b-1", "bf1", 1, 2), "a1", 1, 5);
    let (call, four) = shown(call, &pid("b-1", "bf2", 1, 201), "a2", 1, 40);
    assert_eq!((one, three), (RSeq(5), RSeq(6)), "dialog a1 rises by exactly one");
    assert_eq!((two, four), (RSeq(40), RSeq(41)), "dialog a2 rises by exactly one");
    assert_eq!(responder_of(&call, "a1", RSeq(6)), Some(&pid("b-1", "bf1", 1, 2)));
    assert_eq!(responder_of(&call, "a2", RSeq(41)), Some(&pid("b-1", "bf2", 1, 201)));
    assert_eq!(responder_of(&call, "a2", RSeq(5)), None, "a number shown in the other dialog");
}

/// Forks COLLAPSED behind one shown tag share that dialog's sequence: two
/// responder sequences cannot both be shown verbatim in a single dialog.
#[test]
fn forks_collapsed_behind_one_tag_share_that_dialogs_sequence() {
    let (call, one) = shown(representative_call(), &pid("b-1", "bf1", 1, 1), "a1", 1, 5);
    let (call, two) = shown(call, &pid("b-2", "bf2", 1, 1), "a1", 1, 5);
    assert_eq!((one, two), (RSeq(5), RSeq(6)));
    assert_eq!(responder_of(&call, "a1", RSeq(6)), Some(&pid("b-2", "bf2", 1, 1)));
}

#[test]
fn a_shown_sequence_number_is_never_below_one() {
    let (_, s) = shown(representative_call(), &pid("b-1", "bf1", 1, 1), "a1", 1, 0);
    assert_eq!(s, RSeq(1), "RFC 3262 §3: zero is not a sequence number");
}

/// A shown provisional's ladder: sent once, repeated until acknowledged or
/// ceased; an acknowledged one stays acknowledged.
#[test]
fn a_shown_ladder_runs_until_acknowledged_or_ceased() {
    let (mut call, s) = shown(representative_call(), &pid("b-1", "bf1", 1, 1), "a1", 1, 5);
    let emission =
        common::paced_emission(b"SIP/2.0 183", ("10.0.0.7", 5060), Class::ReliableProvisional, 0);
    assert!(start_shown_ladder(&mut call, "a1", s, emission.clone()));
    assert!(!start_shown_ladder(&mut call, "a1", s, emission), "a started ladder stands");
    assert!(shown_emission(&call, "a1", s).is_some());
    cease_shown_ladder(&mut call, "a1", s, Ceased::Final);
    assert!(shown_emission(&call, "a1", s).is_none());
    assert!(acknowledge_shown(&mut call, "a1", s), "a late PRACK still acknowledges it");
    cease_shown_ladder(&mut call, "a1", s, Ceased::Bound);
    assert!(shown_as(&call, "a1", s).unwrap().acknowledged_by_shown(), "acknowledged stands");
    assert!(!acknowledge_shown(&mut call, "a1", s), "acknowledged once");
}

// ── Retained-emission ladders (ADR-0032 X3/X5) ───────────────────────────────

/// Every rung a `Final2xx` emission owes under `deadline`, walked from its
/// first rung the way the executor walks it.
fn rungs_of_a_2xx_under(deadline: Option<std::time::Duration>) -> u32 {
    let mut emission =
        common::paced_emission(common::ANSWERED_2XX, ("192.0.2.10", 5060), Class::Final2xx, 1);
    let mut rungs = 1;
    while emission.advance(deadline).is_some() {
        rungs += 1;
        assert!(rungs < 100, "a bounded ladder must stop");
    }
    rungs
}

/// RFC 3261 §13.3.1.4 ceases at Timer L whatever the deployment's ACK
/// deadline: a 60 s policy does not add rungs past 64·T1, a 10 s one cuts
/// the ladder short, and no deadline at all is the class's own bound.
#[test]
fn a_2xx_ladder_never_runs_past_timer_l() {
    use std::time::Duration;
    let timer_l = rungs_of_a_2xx_under(None);
    assert_eq!(timer_l, 10, "rungs at 0.5, 1.5, 3.5, 7.5, then every 4 s to 31.5 s");
    assert_eq!(
        rungs_of_a_2xx_under(Some(Duration::from_secs(60))),
        timer_l,
        "a later deadline adds no rung"
    );
    assert_eq!(
        rungs_of_a_2xx_under(Some(Duration::from_secs(10))),
        4,
        "a sooner deadline cuts the ladder short"
    );
}

/// `Scope::Provisionals` names every reliable provisional and no 2xx: the
/// final toward the caller ends the setup's §3 ladders and leaves a 2xx still
/// awaiting its ACK — on either face — repeating.
#[test]
fn the_provisionals_scope_leaves_every_unacked_2xx_alone() {
    let mut call = representative_call();
    call.b_legs[0].dialogs.by_tags_mut(B_LOCAL_TAG, B_TAG).unwrap().pending_reinvite_2xx =
        Some(Unacked2xx {
            dialog_tag: B_TAG.into(),
            cseq: 4002,
            emission: common::paced_emission(
                common::ANSWERED_2XX,
                ("203.0.113.42", 5060),
                Class::Final2xx,
                1,
            ),
        });
    let (call, s) = shown(call, &pid("b-1", B_TAG, 1, 7), A_TAG, 1, 1);

    let provisionals = obligations_in(&call, &Scope::Provisionals);
    assert_eq!(provisionals, vec![Obligation::AwaitPrack { tag: A_TAG.into(), rseq: s }]);

    let everything = obligations_in(&call, &Scope::Call);
    assert_eq!(
        everything.len(),
        3,
        "the a-leg answer, the b-leg re-INVITE 2xx and the provisional"
    );
    assert!(everything.iter().filter(|o| matches!(o, Obligation::AckOf2xx { .. })).count() == 2);
}

/// Every mark bumps the ordinal and carries it; the ring append and the CDR
/// append stamp whatever count stands at that instant, so an entry written
/// between two decisions keeps the first one's ordinal after the second lands.
#[test]
fn marks_number_the_decisions_and_stamp_what_follows() {
    let mut call = representative_call();
    call.decision_log.clear();
    call.decision_ordinal = 0;
    call.b_legs[0].messages = MessageRing::default();
    let entry = || MessageEntry {
        seq: 0,
        at_ms: 5,
        direction: MessageDirection::Relayed,
        method: "INVITE".into(),
        cseq: 1,
        code: None,
        to_tag: None,
        decision_ordinal: 99,
        headers: Vec::new(),
        turn: 0,
    };
    let cdr = || CdrEvent {
        event_type: CdrEventType::InviteSent,
        timestamp: 5,
        leg_id: "b-1".into(),
        status_code: None,
        reason: None,
        decision_ordinal: 99,
    };

    let call = record_message(call, "b-1", 8, entry());
    let call = add_cdr_event(call, cdr());
    let call = mark_decision(call, 10, DecisionKind::Route, Some("a".into()), Some("first".into()));
    let call = record_message(call, "b-1", 8, entry());
    let call = add_cdr_event(call, cdr());
    let call = mark_decision(call, 20, DecisionKind::FailoverRoute, Some("b-1".into()), None);
    let call = record_message(call, "b-1", 8, entry());
    let call = add_cdr_event(call, cdr());

    assert_eq!(call.decision_ordinal, 2);
    assert_eq!(
        call.decision_log,
        vec![
            DecisionMark {
                ordinal: 1,
                at_ms: 10,
                kind: DecisionKind::Route,
                leg_id: Some("a".into()),
                label: Some("first".into()),
            },
            DecisionMark {
                ordinal: 2,
                at_ms: 20,
                kind: DecisionKind::FailoverRoute,
                leg_id: Some("b-1".into()),
                label: None,
            },
        ]
    );
    let ring: Vec<u32> =
        call.b_legs[0].messages.entries.iter().map(|e| e.decision_ordinal).collect();
    assert_eq!(ring, vec![0, 1, 2]);
    let events: Vec<u32> =
        call.cdr_events.iter().rev().take(3).rev().map(|e| e.decision_ordinal).collect();
    assert_eq!(events, vec![0, 1, 2]);
}

/// Every entry appended carries the counter's turn, whatever the entry held;
/// the seal moves the counter past every turn, one that appended nothing
/// included: one turn, one number.
#[test]
fn a_turn_numbers_its_entries_and_the_seal_moves_past_it() {
    let mut call = representative_call();
    call.a_leg.messages = MessageRing::default();
    call.b_legs[0].messages = MessageRing::default();
    call.message_seq = 0;
    call.message_turn = 1;
    let entry = || MessageEntry {
        seq: 0,
        at_ms: 5,
        direction: MessageDirection::Received,
        method: "INVITE".into(),
        cseq: 1,
        code: None,
        to_tag: None,
        decision_ordinal: 0,
        headers: Vec::new(),
        turn: 99,
    };

    let call = record_message(call, "a", 8, entry());
    let call = record_message(call, "b-1", 8, entry());
    let call = seal_turn(call);
    assert_eq!(call.message_turn, 2, "past the turn that recorded two entries");
    let call = seal_turn(call);
    assert_eq!(call.message_turn, 3, "a turn that recorded nothing takes a number too");
    let call = record_message(call, "a", 8, entry());
    let call = seal_turn(call);
    assert_eq!(call.message_turn, 4);

    let turns = |leg: &Leg| leg.messages.entries.iter().map(|e| e.turn).collect::<Vec<u32>>();
    assert_eq!(turns(&call.a_leg), vec![1, 3]);
    assert_eq!(turns(&call.b_legs[0]), vec![1]);
}

/// The first termination's record stands: a second write under another
/// cause changes nothing, and the cut is taken once — the ring's last seq at
/// the seal, never moved by a later seal.
#[test]
fn the_first_termination_is_recorded_once_and_cut_once() {
    let mut call = representative_call();
    call.termination = None;
    call.message_seq = 4;
    assert_eq!(seal_termination_seq(call.clone()).termination, None, "nothing to cut");

    let call = record_termination(call, 10, TerminationCause::RemoteCancel, Some("a".into()));
    let call = record_termination(call, 20, TerminationCause::Supervisor, None);
    assert_eq!(
        call.termination,
        Some(Termination {
            at_ms: 10,
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
            last_seq: 0,
        })
    );
    let mut call = seal_termination_seq(call);
    assert_eq!(call.termination.as_ref().unwrap().last_seq, 4, "cut at the seal");
    call.message_seq = 7;
    let call = seal_termination_seq(call);
    assert_eq!(call.termination.as_ref().unwrap().last_seq, 4, "a later seal moves nothing");
}

/// RFC 3261 §14.1 / RFC 6026 *Accepted* — every mark that keeps an INVITE
/// transaction open on a dialog makes a newcomer INVITE glare: a relayed
/// INVITE awaiting its final, a re-INVITE 2xx this side sent, the call's own
/// answer, and a 2xx taken here whose ACK has not been relayed yet. A dialog
/// carrying none of the four is free for a new INVITE.
#[test]
fn every_open_invite_transaction_mark_makes_a_newcomer_glare() {
    let pristine = |call: &Call| {
        let mut d = b_dialog(call).clone();
        d.inbound_pending_requests.clear();
        d.pending_reinvite_2xx = None;
        d.answered_2xx = None;
        d.awaited_ack_cseq = None;
        d
    };
    let call = representative_call();
    let base = pristine(&call);
    assert!(!invite_transaction_open(&base), "no mark, no glare");

    // Rule 1: a relayed INVITE still awaiting its final response.
    let mut relayed = base.clone();
    relayed.inbound_pending_requests = b_dialog(&call).inbound_pending_requests.clone();
    assert!(
        !relayed.inbound_pending_requests.is_empty(),
        "the fixture dialog carries a pending relayed INVITE"
    );
    assert!(invite_transaction_open(&relayed));

    let unacked = || Unacked2xx {
        dialog_tag: B_TAG.into(),
        cseq: 4002,
        emission: common::paced_emission(
            common::ANSWERED_2XX,
            ("203.0.113.42", 5060),
            Class::Final2xx,
            1,
        ),
    };

    // Rule 2, a 2xx this side sent: a relayed re-INVITE's, and the call's answer.
    let mut reinvite = base.clone();
    reinvite.pending_reinvite_2xx = Some(unacked());
    assert!(invite_transaction_open(&reinvite));

    let mut answered = base.clone();
    answered.answered_2xx = Some(unacked());
    assert!(invite_transaction_open(&answered));

    // Rule 2, a 2xx taken here: its ACK is the peer's, not relayed yet.
    let mut owes_ack = base.clone();
    owes_ack.awaited_ack_cseq = Some(4002);
    assert!(invite_transaction_open(&owes_ack));
}

/// A released call leaves, per shown dialog, the provisionals a PRACK still
/// draws 200 for, under that dialog's Call-ID: one its party acknowledged
/// draws 481 instead, and an unknown tag is an a-facing fork tag, the a-leg's
/// (RFC 3262 §3).
#[test]
fn the_answerable_shown_provisionals_are_grouped_per_shown_dialog() {
    let (call, a1) = shown(settled_call(), &pid("b-1", "bf1", 1, 4711), "a1", 1, 9_000);
    let (call, a2) = shown(call, &pid("b-1", "bf1", 1, 4712), "a1", 1, 9_000);
    let (mut call, f2) = shown(call, &pid("b-1", "bf2", 1, 4711), "a2", 1, 7_000);
    acknowledge_shown(&mut call, "a1", a1);
    assert_eq!(
        answerable_shown(&call),
        vec![
            AnswerableShown {
                call_id: "call-id-deadbeef@example.com".into(),
                tag: "a1".into(),
                racks: vec![(a2, CSeq(1))],
            },
            AnswerableShown {
                call_id: "call-id-deadbeef@example.com".into(),
                tag: "a2".into(),
                racks: vec![(f2, CSeq(1))],
            },
        ],
    );
}

/// Only a dialog's first description-carrying provisional carries the offer
/// (RFC 3264 §4): a later one is described after it.
#[test]
fn only_the_first_description_of_a_dialog_is_its_offer() {
    let mut call = settled_call();
    assert!(!described_before(&call, &pid("b-1", "bf1", 1, 4711)));
    record_provisional(&mut call, pid("b-1", "bf1", 1, 4711), true);
    assert!(described_before(&call, &pid("b-1", "bf1", 1, 4712)), "a later one of that dialog");
    assert!(!described_before(&call, &pid("b-1", "bf2", 1, 4712)), "another fork's first");
    assert!(!described_before(&call, &pid("b-1", "bf1", 2, 4712)), "another INVITE's first");
}

/// A CANCEL the transaction layer answered 487 before the recorded 2xx left
/// (RFC 3261 §9.2): the record comes back to what the wire carried. The 2xx
/// is kept as `answer_preempted` only; no dialog awaits its ACK, and the
/// message ring no longer lists it as sent.
#[test]
fn a_cancelled_invite_takes_back_the_2xx_that_never_left() {
    let mut call = representative_call();
    call.a_leg.dialogs.by_tags_mut(A_TAG, ALICE_TAG).unwrap().awaited_ack_cseq = Some(1);
    let entry = |seq, direction, code: Option<u16>| MessageEntry {
        seq,
        at_ms: 0,
        direction,
        method: "INVITE".into(),
        cseq: 1,
        code,
        to_tag: None,
        decision_ordinal: 0,
        headers: Vec::new(),
        turn: seq,
    };
    call.a_leg.messages.entries = vec![
        entry(1, MessageDirection::Received, None),
        entry(2, MessageDirection::Relayed, Some(180)),
        entry(3, MessageDirection::Relayed, Some(200)),
    ];

    let call = record_cancelled_invite(call, "a");

    let a = &call.a_leg;
    assert_eq!(a.invite_final_sent, Some(487));
    assert_eq!(a.answer_preempted, Some(200));
    assert_eq!(a.state(), LegState::Terminated, "her INVITE's final is the 487");
    assert!(a.dialogs.confirmed().is_none(), "no dialog stands confirmed");
    assert!(a.dialogs.iter().all(|d| d.answered_2xx.is_none()), "no ACK ladder stands");
    assert!(a.dialogs.iter().all(|d| d.awaited_ack_cseq.is_none()), "no ACK is awaited");
    let ring: Vec<_> = a.messages.entries.iter().map(|e| e.seq).collect();
    assert_eq!(ring, [1, 2], "the ring holds what reached the wire");
}

/// RFC 3261 §12.2: a received request names its dialog by (To-tag, From-tag),
/// a response by (From-tag, To-tag) — the same lookup on either leg — and a
/// message missing either tag names none.
#[test]
fn a_message_names_its_dialog_by_direction() {
    let call = representative_call();
    let a = &call.a_leg.dialogs;
    assert_eq!(a.of_request(Some(A_TAG), Some(ALICE_TAG)).unwrap().our_tag, A_TAG);
    assert_eq!(a.of_response(Some(ALICE_TAG), Some(A_TAG)).unwrap().our_tag, A_TAG);
    assert!(a.of_request(Some(ALICE_TAG), Some(A_TAG)).is_none(), "the reverse names none");
    assert!(a.of_request(None, Some(ALICE_TAG)).is_none(), "an untagged To names none");
    let b = &call.b_legs[0].dialogs;
    assert_eq!(b.of_request(Some(B_LOCAL_TAG), Some(B_TAG)).unwrap().peer_tag, B_TAG);
    assert_eq!(b.of_response(Some(B_TAG), Some(B_LOCAL_TAG)).unwrap().peer_tag, B_TAG);
}

/// The dialog a request toward a leg rides when none is named: the confirmed
/// one; else the leg's only live dialog; none while two early forks leave the
/// choice open, and none among dialogs that ended or retired.
#[test]
fn the_carrying_dialog_is_the_confirmed_or_the_only_live_one() {
    let early = |tag: &str| Dialog::new("ours", tag, DialogState::Early, 1);
    let one: Dialogs = vec![early("f1")].into();
    assert_eq!(one.carrying().map(|d| d.peer_tag.as_str()), Some("f1"));
    let two: Dialogs = vec![early("f1"), early("f2")].into();
    assert!(two.carrying().is_none(), "two early forks name no carrying dialog");
    let answered: Dialogs = vec![
        Dialog { state: DialogState::Retired(RetiredDialog::default()), ..early("f1") },
        Dialog { state: DialogState::Confirmed, ..early("f2") },
    ]
    .into();
    assert_eq!(answered.carrying().map(|d| d.peer_tag.as_str()), Some("f2"));
    let ended: Dialogs =
        vec![Dialog { state: DialogState::Ended(DialogEnd::PeerBye), ..early("f1") }].into();
    assert!(ended.carrying().is_none());
}

/// A relay open in an early dialog the answer retired is found through that
/// dialog's tags, and closing it leaves the retired record in place (RFC 3261
/// §8.1.3.3).
#[test]
fn a_retired_dialogs_relay_is_found_by_its_tags_and_closed_in_place() {
    let mut call = settled_call();
    let open = PendingRequest {
        source_call_id: "cid".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        offered: true,
        ..PendingRequest::new("UPDATE", 3, 3, Direction::FromA)
    };
    let lost = Dialog {
        state: DialogState::Retired(RetiredDialog { open_relays: vec![open], release: None }),
        ..Dialog::new(B_LOCAL_TAG, "lost-fork", DialogState::Early, 3)
    };
    call.b_legs[0].dialogs.insert(lost);
    let (holder, pending) =
        relayed_pending(&call, "b-1", Some("lost-fork"), Some(B_LOCAL_TAG), 3).expect("found");
    let id = DialogId { our_tag: B_LOCAL_TAG.into(), peer_tag: "lost-fork".into() };
    assert_eq!(holder, PendingHolder::Retired(id.clone()));
    assert_eq!(pending.method, "UPDATE");
    assert!(relayed_pending(&call, "b-1", Some(B_TAG), Some(B_LOCAL_TAG), 3).is_none());

    let call = close_relay(call, "b-1", &holder, 3);
    let kept = call.b_legs[0].dialogs.by_tags(&id.our_tag, &id.peer_tag).expect("kept");
    assert!(kept.retired().is_some_and(|r| r.open_relays.is_empty()));
}

/// A dialog the peer's BYE ended while early was never established: only
/// the dialog a 2xx confirmed is (RFC 3261 §12.1, §13.2.2.4).
#[test]
fn an_early_dialog_the_peers_bye_ended_was_never_established() {
    let mut leg = representative_call().b_legs[0].clone();
    let ours = leg.outbound_invite.clone().expect("a dialled leg").our_tag;
    leg.dialogs.retain(|_| false);
    let mut early = Dialog::new(&ours, "fork1", DialogState::Ended(DialogEnd::EarlyPeerBye), 1);
    early.ack_branch = None;
    leg.dialogs.insert(early);
    leg.dialogs.insert(Dialog::new(&ours, "fork2", DialogState::Confirmed, 1));
    assert_eq!(leg.dialogs.established().map(|d| d.peer_tag.as_str()), Some("fork2"));
    assert_eq!(leg.state(), LegState::Confirmed);
}
