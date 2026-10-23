//! `shift_instants` moves every absolute instant of the record, and nothing
//! else. The net is the field names: an integer whose key reads as an instant
//! (`*_at`, `*_at_ms`, `timestamp`) moves by the offset, every other integer
//! stays, so an instant field added to the model is caught here unshifted.

use call::helpers::shift_instants;
use call::model::*;
use serde_json::Value;

use crate::common::representative_call;

/// The representative call with every instant-bearing slice the fixture leaves
/// empty filled in: a transfer, a termination, an own NOTIFY and a message
/// ring entry on each leg.
fn every_instant_present() -> Call {
    let mut call = representative_call();
    call.transfer = Some(TransferState {
        phase: TransferPhase::CRinging,
        referrer_leg_id: "b-1".into(),
        refer_to_uri: "sip:carol@example.com".into(),
        effective_refer_to_uri: None,
        callback_context: None,
        c_leg_id: Some("b-2".into()),
        refer_cseq: Some(4003),
        notify_event: "refer".into(),
        started_at_ms: 1_779_440_060_000,
        last_c_leg_provisional_status: None,
        c_initial_sdp: None,
    });
    call.termination = Some(Termination {
        at_ms: 1_779_440_070_000,
        cause: TerminationCause::RemoteBye,
        by_leg: Some("a".into()),
        last_seq: 0,
    });
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        let dialog = leg.dialogs.confirmed_mut().expect("each fixture leg holds its dialog");
        dialog.subscriptions = vec![Subscription {
            notify: Some(NotifyInFlight { cseq: 9000, sent_at_ms: 1_779_440_065_000 }),
            ..Subscription::new("refer", None)
        }];
        leg.messages.entries.push(MessageEntry {
            seq: 1,
            at_ms: 1_779_440_042_005,
            direction: MessageDirection::Received,
            method: "INVITE".into(),
            cseq: 1,
            code: None,
            to_tag: None,
            decision_ordinal: 0,
            headers: Vec::new(),
            turn: 1,
        });
    }
    call
}

/// Whether `key` names an absolute instant.
fn names_an_instant(key: &str) -> bool {
    key.ends_with("_at") || key.ends_with("_at_ms") || key == "at_ms" || key == "timestamp"
}

/// Every integer leaf of `value` outside the opaque `ext` slots, as
/// `(path, key, value)`.
fn integer_leaves(value: &Value, path: &str, key: &str, out: &mut Vec<(String, String, i64)>) {
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push((path.to_string(), key.to_string(), i));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                integer_leaves(item, &format!("{path}[{i}]"), key, out);
            }
        }
        Value::Object(fields) => {
            for (k, v) in fields.iter().filter(|(k, _)| k.as_str() != "ext") {
                integer_leaves(v, &format!("{path}.{k}"), k, out);
            }
        }
        _ => {}
    }
}

fn leaves(call: &Call) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    integer_leaves(&serde_json::to_value(call).expect("a call serialises"), "", "", &mut out);
    out
}

#[test]
fn every_instant_moves_by_the_offset_and_nothing_else_moves() {
    const OFFSET: i64 = -61_234;
    let before = every_instant_present();
    let mut after = before.clone();
    shift_instants(&mut after, OFFSET);

    let (before, after) = (leaves(&before), leaves(&after));
    assert_eq!(before.len(), after.len(), "the shift changes no shape");
    let mut instants = Vec::new();
    for ((path, key, was), (_, _, now)) in before.iter().zip(&after) {
        let expected = if names_an_instant(key) { was + OFFSET } else { *was };
        assert_eq!(*now, expected, "{path} moved by {} (offset {OFFSET})", now - was);
        if names_an_instant(key) {
            instants.push(path.as_str());
        }
    }
    for kind in [
        "fire_at",
        "created_at",
        "timestamp",
        "decision_log",
        "termination",
        "transfer",
        "reroute",
        "sent_at_ms",
        "subscriptions",
        "messages",
    ] {
        assert!(
            instants.iter().any(|p| p.contains(kind)),
            "the fixture holds an instant under {kind}: {instants:?}",
        );
    }
}

#[test]
fn a_zero_offset_changes_nothing() {
    let before = every_instant_present();
    let mut after = before.clone();
    shift_instants(&mut after, 0);
    assert_eq!(after, before);
}
