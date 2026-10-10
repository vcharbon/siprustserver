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
        method: "OPTIONS".into(),
        outbound_cseq: 42,
        inbound_cseq: 42,
        source_vias: vec![],
        source_call_id: "cid".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        direction: Direction::FromB,
        cancelled: false,
        offered_100rel: false,
        offered: false,
        source_timestamp: None,
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

#[test]
fn termination_resolution() {
    let call = representative_call();
    // Confirmed legs with no byeDisposition are not yet resolved.
    assert!(!is_fully_resolved(&call));

    let call = set_bye_disposition(call, "a", ByeDisposition::ByeConfirmed);
    let call = set_bye_disposition(call, "b-1", ByeDisposition::ByeReceived);
    assert!(is_fully_resolved(&call));
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
fn timer_replace_by_id() {
    let existing = representative_call().timers;
    let n = existing.len();
    let replaced = replace_timer_by_id(
        existing,
        TimerEntry {
            id: "timer-no-answer-a".into(),
            timer_type: TimerType::NoAnswer,
            fire_at: 9_999,
            leg_id: Some("a".into()),
        },
    );
    assert_eq!(replaced.len(), n); // replaced, not appended
    let t = replaced.iter().find(|t| t.id == "timer-no-answer-a").unwrap();
    assert_eq!(t.fire_at, 9_999);

    // A brand-new id appends.
    let appended = replace_timer_by_id(
        replaced,
        TimerEntry {
            id: "timer-new".into(),
            timer_type: TimerType::Keepalive,
            fire_at: 1,
            leg_id: None,
        },
    );
    assert_eq!(appended.len(), n + 1);
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

// ── the a-facing reliable-provisional sequence (RFC 3262) ───────────────────

#[test]
fn a_fresh_reliable_provisional_takes_the_next_number() {
    let (call, first) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, false);
    assert_eq!(first, 9_000, "the first relayed provisional takes the random start");
    let (call, second) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 4712, 9_000, false, false);
    assert_eq!(second, 9_001, "RFC 3262 §4: the next in this dialog is greater by exactly one");
    assert_eq!(call.reliable_provisionals.len(), 2);
    assert_eq!(b_rseq_for(&call, "a1", 9_001), Some(("b-1", 4712)));
}

#[test]
fn relaying_the_same_reliable_provisional_again_is_the_same_number() {
    let (call, first) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, false);
    let (call, again) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, false);
    assert_eq!(again, first, "a retransmission is a retransmission, not a new provisional");
    assert_eq!(call.reliable_provisionals.len(), 1);
}

/// `representative_call` deliberately seeds pending INVITE snapshots for
/// round-trip coverage; the reliable-provisional books are only readable on a
/// call with no in-dialog INVITE in flight, so these start from a settled one.
fn settled_call() -> Call {
    let mut call = representative_call();
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        for d in leg.dialogs.iter_mut() {
            d.inbound_pending_requests.clear();
        }
    }
    call
}

/// The a-facing `RAck` tokens a well-formed PRACK carries for `rseq` under the
/// initial INVITE's CSeq.
fn rack(rseq: i64, cseq: i64) -> RAckTokens {
    RAckTokens { rseq, cseq, names_invite: true }
}

/// RFC 3262 §4: a PRACK matching no unacknowledged reliable provisional takes
/// 481, and the a-facing sequence is this stack's own — so what the CALLEE
/// numbered says nothing about whether the caller's RAck acknowledges anything.
#[test]
fn an_rseq_this_dialog_never_showed_acknowledges_nothing() {
    let (call, shown) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 13_213_449, 625_707, false, false);
    assert_eq!(shown, 625_707, "the caller is shown this stack's number, not the callee's");
    assert!(
        !unacknowledgeable_rack(&call, "a", "a1", rack(625_707, 1)),
        "the number it was shown acknowledges it"
    );
    assert!(
        unacknowledgeable_rack(&call, "a", "a1", rack(13_213_449, 1)),
        "the CALLEE's number means nothing on this face, however well it means something on the other"
    );
}

/// RFC 3262 §7.2: the `RAck` is three tokens, and a PRACK matches only where
/// all three name the provisional — the right `RSeq` under a CSeq no a-facing
/// INVITE carried, or under a method no reliable provisional answers, names
/// nothing.
#[test]
fn a_shown_rseq_under_the_wrong_cseq_or_method_acknowledges_nothing() {
    let (call, shown) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 7, 4711, 9_000, false, false);
    assert!(!unacknowledgeable_rack(&call, "a", "a1", rack(shown, 1)));
    assert!(
        unacknowledgeable_rack(&call, "a", "a1", rack(shown, 101)),
        "the CSeq token is a stranger"
    );
    assert!(
        unacknowledgeable_rack(&call, "a", "a1", rack(shown, 7)),
        "the b-leg INVITE's CSeq is the callee's number, not the caller's"
    );
    assert!(
        unacknowledgeable_rack(
            &call,
            "a",
            "a1",
            RAckTokens { rseq: shown, cseq: 1, names_invite: false }
        ),
        "no reliable provisional answers anything but an INVITE"
    );
}

/// Another early dialog's number is another dialog's business: the ladders are
/// independent (§3, errata 4600), so one dialog's RSeq acknowledges nothing in
/// the next.
#[test]
fn an_rseq_from_another_early_dialog_acknowledges_nothing_here() {
    let (call, _) = assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 1, 5, false, false);
    let (call, other) = assign_a_rseq(call, "a2", 1, "b-1", "bf2", 1, 200, 40, false, false);
    assert!(unacknowledgeable_rack(&call, "a", "a1", rack(other, 1)));
    assert!(!unacknowledgeable_rack(&call, "a", "a2", rack(other, 1)));
}

/// Every face's numbering is this stack's own — a reliable provisional leaves
/// toward a face only under a minted number, whichever end sent the INVITE —
/// so each face refuses a PRACK naming nothing, whether or not its dialog has
/// shown a reliable provisional: a masking profile strips the `RSeq` before
/// the caller sees it, and a PRACK into that dialog names nothing all the
/// same. A leg the call does not have owns nothing.
#[test]
fn each_face_refuses_and_it_refuses_into_a_dialog_shown_no_reliable_provisional() {
    assert!(owns_rseq_numbering(&settled_call(), "a"));
    assert!(owns_rseq_numbering(&settled_call(), "b-1"));
    assert!(!owns_rseq_numbering(&settled_call(), "b-9"));
    assert!(unacknowledgeable_rack(&settled_call(), "a", "a1", rack(999, 1)));
    assert!(unacknowledgeable_rack(&settled_call(), "b-1", "bt1", rack(999, 1)));
    assert!(!unacknowledgeable_rack(&settled_call(), "b-9", "bt1", rack(999, 1)));
    // A callee re-INVITE's provisional is shown on the b-face under the b-leg
    // dialog's own tag, and bob's PRACK is matched there.
    let (call, shown) =
        assign_a_rseq(settled_call(), "bt1", 3, "a", "alice-tag", 4, 9271, 700, false, false);
    assert!(!unacknowledgeable_rack(&call, "b-1", "bt1", rack(shown, 3)));
    assert!(
        unacknowledgeable_rack(&call, "b-1", "bt1", rack(9271, 3)),
        "alice's own number means nothing to bob"
    );
    assert!(
        unacknowledgeable_rack(&call, "a", "a1", rack(shown, 3)),
        "the b-face's ladder is not the a-face's"
    );
}

/// RFC 3262 §3 admits a reliable provisional toward a relayed request's
/// originator only for an INVITE, and only one whose originator offered
/// `100rel` itself; the request this stack built toward the target may offer
/// it where the originator did not. A snapshot hydrated without the offer
/// recorded admits nothing.
#[test]
fn a_relayed_provisional_is_reliable_only_to_an_invite_whose_originator_offered_100rel() {
    let pending = |method: &str, offered_100rel: bool| PendingRequest {
        method: method.to_string(),
        outbound_cseq: 2,
        inbound_cseq: 2,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        direction: Direction::FromA,
        cancelled: false,
        offered_100rel,
        offered: false,
        source_timestamp: None,
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

/// A relayed re-INVITE snapshot on the b-leg dialog: the request left toward
/// bob under `outbound_cseq`, offering `100rel` or not.
fn relayed_reinvite(mut call: Call, outbound_cseq: i64, cancelled: bool) -> Call {
    let d = call.b_legs[0].dialogs.by_tags_mut(B_LOCAL_TAG, B_TAG).expect("the b-leg dialog");
    d.inbound_pending_requests.push(PendingRequest {
        method: "INVITE".to_string(),
        outbound_cseq,
        inbound_cseq: 7,
        source_vias: Vec::new(),
        source_call_id: String::new(),
        source_from: String::new(),
        source_to: String::new(),
        direction: Direction::FromA,
        cancelled,
        offered_100rel: true,
        offered: false,
        source_timestamp: None,
    });
    call
}

/// The books are complete on both faces, so an in-dialog INVITE in flight
/// proves nothing about a PRACK: what this stack showed is what it recorded,
/// and a `RAck` naming none of it is refused whatever else is pending.
#[test]
fn a_pending_reinvite_does_not_shield_an_unshown_rack() {
    let (call, _) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 13_213_449, 625_707, false, false);
    assert!(
        unacknowledgeable_rack(&call, "a", "a1", rack(13_213_449, 1)),
        "settled: the books are complete"
    );
    assert!(unacknowledgeable_rack(
        &relayed_reinvite(call.clone(), 2, false),
        "a",
        "a1",
        rack(13_213_449, 1)
    ));
    assert!(unacknowledgeable_rack(
        &relayed_reinvite(call, 2, true),
        "a",
        "a1",
        rack(13_213_449, 1)
    ));
}

/// RFC 3262 §3's give-up rejects the ORIGINAL REQUEST: the provisional of a
/// relayed re-INVITE still pending toward its target names that transaction,
/// while the initial INVITE's — no pending relay, the setup is the call — and
/// a transaction already CANCELled or resolved name none.
#[test]
fn the_give_up_finds_the_pending_reinvite_a_provisional_answers() {
    // Bob's 183 to the re-INVITE this stack sent him as CSeq 2.
    let (call, shown) =
        assign_a_rseq(settled_call(), "a1", 7, "b-1", "bf1", 2, 4711, 900, false, false);
    assert_eq!(
        pending_invite_answered_by(&call, "a1", shown),
        None,
        "no relay pending: the setup's own"
    );
    assert_eq!(
        pending_invite_answered_by(&relayed_reinvite(call.clone(), 2, false), "a1", shown),
        Some(("b-1".to_string(), 2)),
    );
    assert_eq!(
        pending_invite_answered_by(&relayed_reinvite(call.clone(), 2, true), "a1", shown),
        None,
        "a CANCELled transaction has its final already",
    );
    assert_eq!(
        pending_invite_answered_by(&relayed_reinvite(call.clone(), 3, false), "a1", shown),
        None,
        "another transaction's snapshot is not this provisional's",
    );
    assert_eq!(pending_invite_answered_by(&call, "a1", shown + 1), None, "a number never minted");
}

/// A provisional this stack PRACKed itself is on the books once: the first
/// acknowledgement records it, a repeat is recognised as the responder's
/// retransmission (RFC 3262 §4), and another provisional — a new `RSeq`, a
/// later INVITE transaction, another fork's tag — is its own.
#[test]
fn a_stack_pracked_provisional_is_acknowledged_once() {
    let call = settled_call();
    assert!(!pracked_provisional(&call, "b-1", "bf1", 2, 4711));
    let (call, first) = record_pracked_provisional(call, "b-1", "bf1", 2, 4711, false);
    assert!(first, "the first acknowledgement is the one to send");
    assert!(pracked_provisional(&call, "b-1", "bf1", 2, 4711));
    let (call, again) = record_pracked_provisional(call, "b-1", "bf1", 2, 4711, false);
    assert!(!again, "a repeat is a retransmission, not a second PRACK");
    assert_eq!(call.pracked_provisionals.len(), 1);
    assert!(
        !pracked_provisional(&call, "b-1", "bf1", 2, 4712),
        "the next RSeq is a new provisional"
    );
    assert!(
        !pracked_provisional(&call, "b-1", "bf1", 3, 4711),
        "a later INVITE restarts the sequence (§7.1)"
    );
    assert!(!pracked_provisional(&call, "b-1", "bf2", 2, 4711), "another fork's provisional");
}

/// A relayed reliable provisional is owed its PRACK until someone sends one:
/// the party shown it (its entry acknowledged) or this stack itself. Only the
/// leg's own entries on the INVITE named are read (RFC 3262 §4).
#[test]
fn a_relayed_provisional_stays_owed_until_either_side_acknowledges_it() {
    let (call, a1) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, true);
    let (call, _) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 4712, 9_000, false, false);
    let (call, _) = assign_a_rseq(call, "a2", 1, "b-2", "bf2", 1, 4711, 9_000, false, false);
    let owed: Vec<_> = unacknowledged_relayed_provisionals(&call, "b-1", None)
        .into_iter()
        .map(|o| (o.b_tag, o.invite_cseq, o.rseq, o.responder_sdp))
        .collect();
    assert_eq!(owed, vec![("bf1".to_string(), 1, 4711, true), ("bf1".to_string(), 1, 4712, false)]);
    assert!(
        unacknowledged_relayed_provisionals(&call, "b-1", Some(2)).is_empty(),
        "another INVITE"
    );
    let call = retire_a_rseq(call, "a1", a1);
    assert!(provisional_acknowledged(&call, "b-1", "bf1", 1, 4711), "the caller's PRACK");
    let (call, _) = record_pracked_provisional(call, "b-1", "bf1", 1, 4712, false);
    assert!(provisional_acknowledged(&call, "b-1", "bf1", 1, 4712), "this stack's PRACK");
    assert!(unacknowledged_relayed_provisionals(&call, "b-1", None).is_empty());
    assert_eq!(unacknowledged_relayed_provisionals(&call, "b-2", Some(1)).len(), 1);
}

/// Forks mirrored as DISTINCT a-facing early dialogs each carry their own
/// ladder (RFC 3262 §4, errata 4603/4604), so interleaving them never shows
/// either caller dialog a gap — the failure a single call-wide ladder produces.
#[test]
fn each_a_facing_early_dialog_carries_its_own_ladder() {
    let (call, one) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 1, 5, false, false);
    let (call, two) = assign_a_rseq(call, "a2", 1, "b-1", "bf2", 1, 200, 40, false, false);
    let (call, three) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 2, 5, false, false);
    let (call, four) = assign_a_rseq(call, "a2", 1, "b-1", "bf2", 1, 201, 40, false, false);
    assert_eq!((one, three), (5, 6), "dialog a1 rises by exactly one across the interleave");
    assert_eq!((two, four), (40, 41), "dialog a2 rises by exactly one across the interleave");
    assert_eq!(b_rseq_for(&call, "a1", 6), Some(("b-1", 2)));
    assert_eq!(b_rseq_for(&call, "a2", 41), Some(("b-1", 201)));
    assert_eq!(b_rseq_for(&call, "a2", 5), None, "a number minted in the other dialog");
    assert_eq!(b_rseq_for(&call, "a1", 99), None, "a number this stack never minted");
}

/// Forks COLLAPSED behind one a-facing tag share that dialog's ladder — the
/// case the mapping exists for, since two callee sequences cannot both be
/// shown verbatim in a single caller dialog.
#[test]
fn forks_collapsed_behind_one_tag_share_that_dialogs_ladder() {
    let (call, one) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 1, 5, false, false);
    let (call, two) = assign_a_rseq(call, "a1", 1, "b-2", "bf2", 1, 1, 5, false, false);
    assert_eq!((one, two), (5, 6), "one caller dialog, one ladder rising by exactly one");
    assert_eq!(b_rseq_for(&call, "a1", 5), Some(("b-1", 1)));
    assert_eq!(b_rseq_for(&call, "a1", 6), Some(("b-2", 1)), "the PRACK reaches the right fork");
}

/// RFC 3262 §7.1 restarts the callee's `RSeq` at random per INVITE
/// transaction, so a re-INVITE may state a number the initial INVITE already
/// used on the same leg. That is a NEW provisional, never a retransmission.
#[test]
fn the_same_b_sequence_on_a_later_transaction_is_a_new_provisional() {
    let (call, initial) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, false);
    let (call, reinvite) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 2, 4711, 9_000, false, false);
    assert_ne!(reinvite, initial, "a later INVITE transaction restarts the callee's sequence");
    assert_eq!(reinvite, initial + 1, "RFC 3262 §4: this dialog's ladder still rises by one");
    assert_eq!(call.reliable_provisionals.len(), 2);
}

#[test]
fn an_a_facing_sequence_number_is_never_below_one() {
    let (_, a_rseq) =
        assign_a_rseq(representative_call(), "a1", 1, "b-1", "bf1", 1, 1, 0, false, false);
    assert_eq!(a_rseq, 1, "RFC 3262 §3: zero is not a sequence number");
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
    call.reliable_provisionals.push(ReliableProvisional {
        a_tag: A_TAG.into(),
        a_rseq: 1,
        a_cseq: 1,
        b_leg_id: "b-1".into(),
        b_tag: B_TAG.into(),
        b_cseq: 1,
        b_rseq: 7,
        acknowledged: false,
        emission: None,
        carried_sdp: false,
        responder_sdp: false,
        responder_offer: None,
    });

    let provisionals = obligations_in(&call, &Scope::Provisionals);
    assert_eq!(provisionals, vec![Obligation::PrackOf { a_tag: A_TAG.into(), a_rseq: 1 }]);

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

/// A released call leaves, per shown dialog, the provisionals its party never
/// PRACKed, under that dialog's Call-ID: an unknown tag is an a-facing fork
/// tag, the a-leg's (RFC 3262 §3).
#[test]
fn the_unacknowledged_shown_provisionals_are_grouped_per_shown_dialog() {
    let (call, a1) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, false, false);
    let (call, a2) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 4712, 9_000, false, false);
    let (call, f2) = assign_a_rseq(call, "a2", 1, "b-1", "bf2", 1, 4711, 7_000, false, false);
    let call = retire_a_rseq(call, "a1", a1);
    assert_eq!(
        unacknowledged_shown(&call),
        vec![
            UnackedShown {
                call_id: "call-id-deadbeef@example.com".into(),
                tag: "a1".into(),
                racks: vec![(a2, 1)],
            },
            UnackedShown {
                call_id: "call-id-deadbeef@example.com".into(),
                tag: "a2".into(),
                racks: vec![(f2, 1)],
            },
        ],
    );
}

/// Only a dialog's first description-carrying provisional carries the offer
/// (RFC 3264 §4): a later one owes its PRACK no answer, whether the first is
/// still owed or was acknowledged already.
#[test]
fn only_the_first_offer_of_a_dialog_is_owed_an_answer() {
    let (call, a1) =
        assign_a_rseq(settled_call(), "a1", 1, "b-1", "bf1", 1, 4711, 9_000, true, true);
    let (call, _) = assign_a_rseq(call, "a1", 1, "b-1", "bf1", 1, 4712, 9_000, true, true);
    let (call, _) = assign_a_rseq(call, "a2", 1, "b-1", "bf2", 1, 4711, 7_000, true, true);
    let call = note_responder_offer(call, "b-1", "bf1", 1, 4711, b"offer-1");
    let call = note_responder_offer(call, "b-1", "bf1", 1, 4712, b"offer-1-again");
    let call = note_responder_offer(call, "b-1", "bf2", 1, 4711, b"offer-2");
    let offers = |call: &Call| -> Vec<(String, i64, Option<Vec<u8>>)> {
        unacknowledged_relayed_provisionals(call, "b-1", None)
            .into_iter()
            .map(|o| (o.b_tag, o.rseq, o.offer))
            .collect()
    };
    assert_eq!(
        offers(&call),
        vec![
            ("bf1".to_string(), 4711, Some(b"offer-1".to_vec())),
            ("bf1".to_string(), 4712, None),
            ("bf2".to_string(), 4711, Some(b"offer-2".to_vec())),
        ],
    );
    let call = retire_a_rseq(call, "a1", a1);
    assert_eq!(
        offers(&call),
        vec![("bf1".to_string(), 4712, None), ("bf2".to_string(), 4711, Some(b"offer-2".to_vec()))],
        "the offer was answered in the caller's PRACK"
    );
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
    assert_eq!(a.state, LegState::Early);
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
        method: "UPDATE".into(),
        outbound_cseq: 3,
        inbound_cseq: 3,
        source_vias: vec![],
        source_call_id: "cid".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        direction: Direction::FromA,
        cancelled: false,
        offered_100rel: false,
        offered: true,
        source_timestamp: None,
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
