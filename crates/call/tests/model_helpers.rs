//! Unit tests locking the pure lens/accessor/timer helpers. No direct TS
//! counterpart (the source exercised these via the deferred CallState tests);
//! analogous to slice-1's `message_helpers.rs` smoke tests.

mod common;

use call::helpers::*;
use call::model::*;
use common::representative_call;

const A_TAG: &str = "b2bua-to-tag-aleg-9876"; // a-leg dialog identity (localTag)
const B_TAG: &str = "bob-to-tag-007"; // b-1 dialog identity (remoteTag)

#[test]
fn find_and_kind_accessors() {
    let call = representative_call();
    assert_eq!(find_leg(&call, "a").unwrap().leg_id, "a");
    assert_eq!(find_b_leg(&call, "b-1").unwrap().leg_id, "b-1");
    assert!(find_b_leg(&call, "nope").is_none());
    assert_eq!(
        find_b_leg_by_call_id(&call, "b-leg-call-id-fedcba@b2bua")
            .unwrap()
            .leg_id,
        "b-1"
    );
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
    let before = call.a_leg.dialogs[0].sip.local_cseq;
    let call = bump_local_cseq(call, "a", A_TAG, 5);
    assert_eq!(call.a_leg.dialogs[0].sip.local_cseq, before + 5);

    let call = update_remote_cseq(call, "b-1", B_TAG, 9999);
    assert_eq!(call.b_legs[0].dialogs[0].ext.remote_cseq, Some(9999));

    assert_eq!(relay_cseq_delta(10, None), 1);
    assert_eq!(relay_cseq_delta(10, Some(7)), 3);
    assert_eq!(relay_cseq_delta(5, Some(9)), 1); // clamped ≥ 1
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
    };
    let call = add_pending_request(call, "b-1", B_TAG, entry);
    let d = &call.b_legs[0].dialogs[0];
    assert!(find_pending_request(d, 42).is_some());

    let call = remove_pending_request(call, "b-1", B_TAG, 42);
    assert!(find_pending_request(&call.b_legs[0].dialogs[0], 42).is_none());
}

#[test]
fn tag_accessors_and_mapping() {
    let call = representative_call();
    assert_eq!(b2bua_tag(&call, "a").as_deref(), Some(A_TAG));
    assert_eq!(remote_tag(&call, "a").as_deref(), Some("alice-from-tag-001"));
    assert_eq!(
        b2bua_tag(&call, "b-1").as_deref(),
        Some("b2bua-from-tag-bleg-5544")
    );
    assert_eq!(remote_tag(&call, "b-1").as_deref(), Some(B_TAG));

    // Duplicate (bLegId, bTag) is a no-op.
    let mapping = TagMapping {
        a_tag: "other".into(),
        b_leg_id: "b-1".into(),
        b_tag: B_TAG.into(),
    };
    let n_before = call.tag_map.len();
    let call = add_tag_mapping(call, mapping);
    assert_eq!(call.tag_map.len(), n_before);
    assert_eq!(find_by_a_tag(&call, A_TAG).unwrap().b_leg_id, "b-1");
    assert!(find_by_b_tag(&call, "b-1", B_TAG).is_some());
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
    let r = call
        .active_rules
        .as_ref()
        .unwrap()
        .iter()
        .find(|r| r.id == "limit-by-subscriber")
        .unwrap();
    assert!(!r.active);
}

#[test]
fn a_leg_invite_cseq_extraction() {
    let call = representative_call();
    assert_eq!(a_leg_invite_cseq_num(&call), 1); // "1 INVITE"
}

#[test]
fn dialog_constructors() {
    let ctx = MakeDialogLegCtx {
        call_id: "cid",
        local_uri: "sip:a@x",
        remote_uri: "sip:b@y",
        local_tag: "lt",
        remote_tag: "rt",
    };
    let empty = make_empty_dialog(&ctx, 1000);
    assert_eq!(empty.sip.local_cseq, 1000);
    assert_eq!(empty.sip.remote_target, "");
    assert_eq!(empty.ext.remote_cseq, None);
    assert!(empty.ext.inbound_pending_requests.is_empty());

    let from_incoming =
        make_dialog_from_incoming(&ctx, 500, vec!["<sip:rr;lr>".into()], 2000);
    assert_eq!(from_incoming.ext.remote_cseq, Some(500));
    assert_eq!(from_incoming.sip.route_set, vec!["<sip:rr;lr>".to_string()]);
    assert_eq!(from_incoming.sip.local_cseq, 2000);
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
