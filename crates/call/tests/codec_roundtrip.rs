//! Codec round-trip + property suite — the Rust port of the meaningful codec
//! properties from `tests/codec/round-trip-property.test.ts` (P1/P2/P3/P5/P6/P7/
//! P8/P14). Parity (the source's `parity` wrapper) is intentionally out of scope
//! for this slice; P10/P11/P13 collapse into compile-time guarantees in Rust
//! (`encode(&Call)` cannot mutate, `decode` returns the typed `Call`). See
//! ADR-0008.

mod common;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use call::{
    CallBodyCodec, CallDecodeError, LegKind, MachineId, MsgpackCodec, PolicyUpdateBody, StateLabel,
};
use common::{arb_call, paced_emission, representative_call};
use proptest::prelude::*;
use sip_retransmit::Class;

#[test]
fn representative_round_trips() {
    let codec = MsgpackCodec::new();
    let call = representative_call();
    let bytes = codec.encode(&call);
    assert!(!bytes.is_empty(), "P14: non-empty output");
    let decoded = codec.decode(&bytes).expect("decode");
    assert_eq!(decoded, call, "P1: round-trip preserves the value");
}

/// A service-owned timer (`TimerType::Service`, the ADR-0016 watchdog seam)
/// rides the replicated `call.timers` ledger like any core timer: the borrowed
/// `Cow<'static, str>` declaration form encodes and decodes back as an owned,
/// **equal** value (the HA takeover-restore path), and the persisted id recipe
/// (`TimerType::timer_id` == `Debug`) is stable per `(service_id, key)` [+ leg].
#[test]
fn service_timer_entry_round_trips_and_id_recipe_is_stable() {
    use call::{TimerEntry, TimerType};

    let t = TimerType::service(MachineId::new("routing"), "timer18x");
    assert_eq!(t.timer_id(None), "Service:routing:timer18x");
    assert_eq!(t.timer_id(Some("b-1")), "Service:routing:timer18x:b-1");
    // Distinct keys mint distinct ids (no collision in the driver's
    // (call_ref, id) keyspace); same key re-derives the same id (supersede).
    assert_ne!(
        TimerType::service(MachineId::new("routing"), "prack-guard").timer_id(None),
        t.timer_id(None),
    );

    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    call.timers.push(TimerEntry {
        id: t.timer_id(None),
        timer_type: t.clone(),
        fire_at: 1_234_567,
        leg_id: None,
    });
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "service timer survives the replication codec");
    let restored = decoded.timers.last().unwrap();
    assert_eq!(
        restored.timer_type, t,
        "owned-Cow deserialisation compares equal to the borrowed declaration"
    );
}

/// Replication sanity: the release-event `subscriptions` and
/// the in-flight `reroute` slice are ordinary replicated `Call` fields — a
/// takeover node must keep honoring the subscription and be able to resume /
/// tear down a mid-reroute call. Round-trips through the production msgpack
/// codec across all three shapes (none / subscribed-idle / mid-reroute).
#[test]
fn release_subscriptions_and_reroute_round_trip() {
    use call::{ReleaseEventKind, ReroutePhase, RerouteState};

    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    call.subscriptions = Vec::new();
    call.reroute = None;
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "unsubscribed / no-reroute shape");

    call.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
    call.reroute = None;
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "subscribed-idle shape");
    assert!(decoded.subscriptions.contains(&ReleaseEventKind::MaxCallDuration));

    call.reroute = Some(RerouteState {
        phase: ReroutePhase::BLegDialing,
        new_leg_id: "b-2".into(),
        old_leg_id: Some("b-1".into()),
        started_at_ms: 1_779_440_099_000,
    });
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "mid-reroute shape");
}

/// The decision log and the ordinal stamped on events and ring entries are
/// replicated state — a takeover node's record names the decision every
/// message was handled under — so they survive the codec in every shape: no
/// decision yet, one labelled mark, a second mark without a label.
#[test]
fn the_decision_log_round_trips() {
    use call::helpers::{add_cdr_event, mark_decision};
    use call::{CdrEvent, CdrEventType, DecisionKind};

    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    call.decision_log = Vec::new();
    call.decision_ordinal = 0;
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "no decision yet");

    let call =
        mark_decision(call, 1_000, DecisionKind::Route, Some("a".into()), Some("first".into()));
    let call = mark_decision(call, 2_000, DecisionKind::FailoverRoute, Some("b-1".into()), None);
    let call = add_cdr_event(
        call,
        CdrEvent {
            event_type: CdrEventType::InviteSent,
            timestamp: 2_000,
            leg_id: "b-2".into(),
            status_code: None,
            reason: None,
            decision_ordinal: 0,
        },
    );
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "two marks");
    assert_eq!(decoded.decision_ordinal, 2);
    let labels: Vec<Option<&str>> =
        decoded.decision_log.iter().map(|m| m.label.as_deref()).collect();
    assert_eq!(labels, vec![Some("first"), None]);
    assert_eq!(decoded.decision_log[1].ordinal, 2);
    assert_eq!(decoded.cdr_events.last().unwrap().decision_ordinal, 2);
}

/// The final a leg's initial INVITE carries is replicated state — a takeover
/// node must refuse a second final on that transaction exactly as the node
/// that sent the first would — so it survives the replication codec in both
/// shapes (unanswered / answered).
#[test]
fn the_invite_final_sent_fact_round_trips() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    call.a_leg.invite_final_sent = None;
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "unanswered a-leg");
    assert_eq!(decoded.a_leg.invite_final_sent, None);

    call.a_leg.invite_final_sent = Some(487);
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "a-leg carrying its final");
    assert_eq!(decoded.a_leg.invite_final_sent, Some(487));

    // The helper records the first final only.
    let call = call::helpers::record_invite_final(call, "a", 480);
    assert_eq!(call.a_leg.invite_final_sent, Some(487), "the first final stands");
    let call = call::helpers::record_invite_final(call, "b-1", 200);
    assert_eq!(call.b_legs[0].invite_final_sent, Some(200));
}

/// RFC 3262 §7.1: the a-facing reliable-provisional map is replicated state —
/// a PRACK arriving after a takeover must still translate onto the b-leg number
/// it acknowledges, so the map has to survive the replication codec.
#[test]
fn the_reliable_provisional_map_round_trips() {
    use call::ReliableProvisional;

    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    call.reliable_provisionals = Vec::new();
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "no reliable provisional relayed yet");

    call.reliable_provisionals = vec![
        ReliableProvisional {
            a_tag: "a1".into(),
            a_rseq: 9_000,
            b_leg_id: "b-1".into(),
            b_tag: "bf1".into(),
            b_cseq: 1,
            b_rseq: 4711,
            acknowledged: true,
            emission: None,
            a_cseq: Some(1),
        },
        ReliableProvisional {
            a_tag: "a2".into(),
            a_rseq: 40,
            b_leg_id: "b-2".into(),
            b_tag: "bf2".into(),
            b_cseq: 1,
            b_rseq: 1,
            acknowledged: false,
            emission: None,
            a_cseq: Some(1),
        },
    ];
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "both early dialogs' provisionals survive");
    assert_eq!(decoded.reliable_provisionals[1].b_rseq, 1);
    assert!(
        decoded.reliable_provisionals[0].acknowledged,
        "the retirement survives a takeover — the survivor must not re-relay a repeat",
    );

    // A live §3 ladder emission is replicated state too: the exact datagram a
    // takeover node re-sends, its destination, and the rung the ladder stands on.
    let datagram = b"SIP/2.0 183 Session Progress\r\nRSeq: 40\r\n\r\n";
    call.reliable_provisionals[1].emission =
        Some(paced_emission(datagram, ("10.0.0.7", 5060), Class::ReliableProvisional, 3));
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "the retained emission survives byte-exact");
    let emission = decoded.reliable_provisionals[1].emission.as_ref().unwrap();
    assert_eq!(emission.wire(), (&datagram[..], ("10.0.0.7", 5060)));
    assert_eq!(
        emission.repeat(),
        call::Repeat::Paced { class: Class::ReliableProvisional, rung: 3 },
        "the ladder resumes where it stood",
    );
}

/// The replication body is positional: an entry a peer encoded before the
/// a-facing INVITE CSeq was recorded is short of that trailing element — seven
/// elements from the common older peer, whose encoder skipped an absent
/// emission, eight from one with a live ladder — and either hydrates with
/// `a_cseq = None`: the state that admits any CSeq token rather than refusing
/// a PRACK on books that never held the fact.
#[test]
fn a_reliable_provisional_encoded_without_a_cseq_hydrates_without_one() {
    use call::ReliableProvisional;

    #[derive(serde::Serialize)]
    struct BeforeCseqNoEmission<'a> {
        a_tag: &'a str,
        a_rseq: i64,
        b_leg_id: &'a str,
        b_tag: &'a str,
        b_cseq: i64,
        b_rseq: i64,
        acknowledged: bool,
    }
    #[derive(serde::Serialize)]
    struct BeforeCseqWithEmission<'a> {
        a_tag: &'a str,
        a_rseq: i64,
        b_leg_id: &'a str,
        b_tag: &'a str,
        b_cseq: i64,
        b_rseq: i64,
        acknowledged: bool,
        emission: Option<call::RetainedEmission>,
    }
    let common = BeforeCseqNoEmission {
        a_tag: "a1",
        a_rseq: 9_000,
        b_leg_id: "b-1",
        b_tag: "bf1",
        b_cseq: 1,
        b_rseq: 4711,
        acknowledged: false,
    };
    let decoded: ReliableProvisional =
        rmp_serde::from_slice(&rmp_serde::to_vec(&common).unwrap()).unwrap();
    assert_eq!((decoded.a_tag.as_str(), decoded.a_rseq, decoded.b_rseq), ("a1", 9_000, 4711));
    assert_eq!(decoded.emission, None);
    assert_eq!(decoded.a_cseq, None, "no CSeq was recorded, none is invented");

    let with_ladder = BeforeCseqWithEmission {
        a_tag: "a1",
        a_rseq: 9_000,
        b_leg_id: "b-1",
        b_tag: "bf1",
        b_cseq: 1,
        b_rseq: 4711,
        acknowledged: false,
        emission: Some(paced_emission(
            b"SIP/2.0 183 Session Progress\r\nRSeq: 9000\r\n\r\n",
            ("10.0.0.7", 5060),
            Class::ReliableProvisional,
            1,
        )),
    };
    let decoded: ReliableProvisional =
        rmp_serde::from_slice(&rmp_serde::to_vec(&with_ladder).unwrap()).unwrap();
    assert_eq!(
        decoded.emission.as_ref().map(|e| e.repeat()),
        Some(call::Repeat::Paced { class: Class::ReliableProvisional, rung: 1 }),
        "the ladder rides along",
    );
    assert_eq!(decoded.a_cseq, None, "no CSeq was recorded, none is invented");
}

/// RFC 3261 §13.2.2.4: the retained ACK-for-2xx datagram (`emitted_ack`) is
/// replicated dialog state — a takeover node re-ACKs a retransmitted 2xx with
/// the SAME bytes the failed node sent, at the same destination (on a delayed
/// offer they carry the answer) — so it must survive the replication codec
/// byte-exact, in both the retained and the not-yet-retained shape.
#[test]
fn the_retained_emitted_ack_round_trips() {
    use call::{Repeat, RetainedEmission};

    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    if let Some(d) = call.b_legs[0].dialogs.first_mut() {
        d.ext.emitted_ack = None;
    }
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "no ACK emitted yet on the b-leg dialog");

    let datagram = b"ACK sip:bob@10.0.0.7 SIP/2.0\r\nCSeq: 8005 ACK\r\n\r\nanswer";
    if let Some(d) = call.b_legs[0].dialogs.first_mut() {
        d.ext.emitted_ack = Some(RetainedEmission::on_trigger(
            datagram.to_vec(),
            ("10.0.0.7".into(), 5060),
            call::Repeated::request("ACK"),
        ));
    }
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "the retained ACK survives byte-exact");
    let ack = decoded.b_legs[0].dialogs[0].ext.emitted_ack.as_ref().unwrap();
    assert_eq!(
        ack.wire(),
        (&datagram[..], ("10.0.0.7", 5060)),
        "the takeover node re-sends the same bytes at the destination the ACK first took",
    );
    assert_eq!(
        ack.repeat(),
        Repeat::OnTrigger,
        "an ACK is repeated only when a 2xx copy provokes it"
    );
    assert_eq!(
        ack.repeated(),
        &call::Repeated::request("ACK"),
        "the label captured at retention rides with the datagram: a takeover node counts its re-ACK as one",
    );
}

/// RFC 3261 §13.3.1.4: the a-leg 2xx the representative call is still owed an
/// ACK for rides the body with its ladder position, so a takeover node repeats
/// the SAME bytes on the rung the failed node stood on — never from rung one.
#[test]
fn the_retained_answered_2xx_round_trips_with_its_rung() {
    use call::Repeat;

    let codec = MsgpackCodec::new();
    let call = representative_call();
    let answered =
        call.a_leg.dialogs[0].ext.answered_2xx.as_ref().expect("the fixture answered the caller");
    assert_eq!(answered.emission.repeat(), Repeat::Paced { class: Class::Final2xx, rung: 3 });

    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded, call, "the retained 2xx survives byte-exact");
    let restored = decoded.a_leg.dialogs[0].ext.answered_2xx.as_ref().unwrap();
    assert_eq!(
        (restored.dialog_tag.as_str(), restored.cseq),
        ("b2bua-to-tag-aleg-9876", 1),
        "the ACK's key rides with it"
    );
    assert_eq!(restored.emission.wire(), (common::ANSWERED_2XX, ("192.0.2.10", 5060)));
    assert_eq!(
        restored.emission.repeat(),
        Repeat::Paced { class: Class::Final2xx, rung: 3 },
        "the ladder resumes where it stood"
    );
    assert_eq!(
        restored.emission.repeated(),
        &call::Repeated::response("INVITE", 200),
        "the label captured at retention survives: a takeover node's rungs count as INVITE/200",
    );
}

/// PA2 (source paranoid-decode precondition): empty input is a typed error.
#[test]
fn decode_empty_is_error() {
    let codec = MsgpackCodec::new();
    assert!(matches!(codec.decode(&[]), Err(CallDecodeError::Empty)));
}

/// P7 — `Vec<u8>` body integrity across empty / tiny / 1 KiB / 64 KiB.
#[test]
fn p7_binary_integrity_sizes() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    for len in [0usize, 1, 1024, 65536] {
        let body: Vec<u8> = (0..len).map(|i| (i & 0xff) as u8).collect();
        call.a_leg_invite.body = body.clone();
        let decoded = codec.decode(&codec.encode(&call)).unwrap();
        assert_eq!(decoded.a_leg_invite.body, body, "len={len}");
    }
}

/// The one preserved absent/null/value distinction (source `policyUpdateBody`).
#[test]
fn policy_body_three_states_round_trip() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    for v in [None, Some(PolicyUpdateBody::Empty), Some(PolicyUpdateBody::Bytes(vec![1, 2, 3, 4]))]
    {
        call.policy_update_body = v.clone();
        let decoded = codec.decode(&codec.encode(&call)).unwrap();
        assert_eq!(decoded.policy_update_body, v);
    }
}

/// The `emergency` flag (`CallModel.ts` L628–635) is the data-layer prerequisite
/// for the SipRouter `;emerg=1` / `;em=1` URI/Via marker stamping that lets the
/// dispatcher byte-classifier route in-dialog packets into the emergency priority
/// queue without parsing — so it MUST survive the failover/replication round-trip
/// intact.
///
/// Wire position. The TS positional msgpack codec carries it at `FIELD_ORDER`
/// **index 20** (`msgpack.ts` L37: `callRef`=0 … `_topology`=19, `emergency`=20);
/// the protobuf codec encodes it whenever `!== undefined` (`protobuf.ts` L143),
/// under id `optional bool emergency = 24` (`call.proto`). (The older bench
/// definition in `tests/bench/call-codec/codec.ts` uses `id: 21`; the bench and
/// the current proto diverge — cite the proto, not the bench.) Our codec here is
/// `rmp-serde`'s positional/array encoding, so the field is carried by struct
/// position, exactly like the TS msgpack codec.
///
/// States. The three round-tripped here are `None`, `Some(false)`, `Some(true)`:
/// - In production only `true` or absent are ever written: the producer is
///   `SipRouter.ts` L1215 `emergency: isEmergency || undefined`, which coerces a
///   computed `false` to `undefined`. `Some(false)` is therefore exercised here
///   purely for codec robustness, not because the system emits it.
/// - `None` is safe to carry **only** because the project redeploys from scratch
///   (no rolling-upgrade contract — see CLAUDE.md) and the field has always been
///   present in the Rust `Call` struct. It is NOT a back-compat / "wire default"
///   state: under this positional codec an `Option<bool>` that is mid-struct and
///   lacks `#[serde(default)]` does **not** support old-body (field-absent)
///   decode — a body emitted without the field fails to decode (the next field's
///   marker lands where the bool is expected), and a `None` body is not
///   byte-identical to a field-absent one (`None` serializes an explicit nil).
///   Contrast `sm_cursors` below — the genuine back-compat field — which is the
///   LAST field, carries `#[serde(default, skip_serializing_if)]`, and has an
///   explicit field-absent decode assertion.
///
/// The TS source has no dedicated codec test for this field (it is covered only
/// generically by the round-trip property); this pins its contract.
#[test]
fn emergency_three_states_round_trip() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    for v in [None, Some(false), Some(true)] {
        call.emergency = v;
        let decoded = codec.decode(&codec.encode(&call)).unwrap();
        assert_eq!(decoded.emergency, v, "emergency={v:?} must round-trip");
        assert_eq!(decoded, call, "full value preserved with emergency={v:?}");
    }
}

/// ADR-0016 slice 0 — `sm_cursors` is replication-compatible under the positional
/// msgpack codec: a populated map round-trips, an empty map round-trips, and an
/// **old-shape body** (encoded before the field existed) decodes to an empty map.
#[test]
fn sm_cursors_round_trip_and_back_compat() {
    let codec = MsgpackCodec::new();

    // Populated map round-trips.
    let mut call = representative_call();
    call.sm_cursors.insert(MachineId::new("global-call"), StateLabel::new("Active"));
    call.sm_cursors.insert(MachineId::new("transfer"), StateLabel::new("CRinging"));
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded.sm_cursors, call.sm_cursors);
    assert_eq!(decoded, call, "full value preserved with cursors set");

    // Empty map round-trips (and is skipped on the wire).
    let empty = representative_call();
    assert!(empty.sm_cursors.is_empty());
    let decoded_empty = codec.decode(&codec.encode(&empty)).unwrap();
    assert!(decoded_empty.sm_cursors.is_empty());
    assert_eq!(decoded_empty, empty);

    // Old-shape body: an empty `sm_cursors` is skipped by
    // `skip_serializing_if`, so the encoding of a cursors-free call is
    // byte-identical to what an old node (without the field) would emit. It
    // decodes back into an empty map via `#[serde(default)]`.
    let old_bytes = codec.encode(&empty);
    let from_old = codec.decode(&old_bytes).unwrap();
    assert!(from_old.sm_cursors.is_empty(), "absent field decodes to empty");
}

/// GAP-P7-2 — the `relay18x.messages` policy field and the `ONE_PER_VALUE`
/// dedupe ledger are replication-compatible under the positional msgpack codec:
/// an old-shape body (encoded before the trailing fields existed) decodes with
/// the FIRST / empty defaults via `#[serde(default)]`.
#[test]
fn relay18x_messages_fields_decode_from_old_shape_bodies() {
    use call::features::{Relay18xMessages, RelayFirst18xStrategy, RelayFirst18xTo180Feature};

    // Old `RelayFirst18xTo180Feature` carried ONLY `strategy` (one element).
    #[derive(serde::Serialize)]
    struct OldFeature {
        strategy: RelayFirst18xStrategy,
    }
    let old =
        rmp_serde::to_vec(&OldFeature { strategy: RelayFirst18xStrategy::FakePrack }).unwrap();
    let new: RelayFirst18xTo180Feature = rmp_serde::from_slice(&old).unwrap();
    assert_eq!(new.strategy, RelayFirst18xStrategy::FakePrack);
    assert_eq!(new.messages, Relay18xMessages::First, "absent messages defaults to FIRST");

    // Old `RelayFirst18xState` carried `(first_relayed, stored_a_tag)`.
    #[derive(serde::Serialize)]
    struct OldState {
        first_relayed: bool,
        stored_a_tag: Option<String>,
    }
    let old = rmp_serde::to_vec(&OldState { first_relayed: true, stored_a_tag: Some("a1".into()) })
        .unwrap();
    let new: call::RelayFirst18xState = rmp_serde::from_slice(&old).unwrap();
    assert!(new.first_relayed);
    assert_eq!(new.stored_a_tag.as_deref(), Some("a1"));
    assert!(new.relayed_values.is_empty(), "absent dedupe ledger decodes empty");
}

/// ADR-0017 X4 — the numbering-plan **reroute remainder** rides the existing
/// opaque `callback_context` string (and per-service `ext`), so it is part of the
/// replicated `Call` body and survives failover by construction: no new wire
/// field, no positional-codec change. This is the data-layer proof that the plan
/// is "saved by the HA mechanisms".
#[test]
fn reroute_plan_in_callback_context_survives_replication() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();

    // The opaque blob a real HTTP backend would round-trip as a token; here it
    // carries the remaining attempts + the exhaustion treatment.
    let plan = serde_json::json!({
        "routes": [
            {"destination": {"host": "10.0.0.2", "port": 5070}, "new_to": "sip:+1@b"}
        ],
        "on_exhausted": {"action": "reject", "code": 603, "reason": "Declined",
                         "update_headers": {"Reason": "Q.850;cause=21"}}
    })
    .to_string();
    call.callback_context = Some(plan.clone());
    call.ext
        .get_or_insert_with(Default::default)
        .insert("numbering-plan".to_string(), serde_json::json!({"attempt": 1}));

    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded.callback_context.as_deref(), Some(plan.as_str()));
    assert_eq!(
        decoded.ext.as_ref().and_then(|e| e.get("numbering-plan")),
        Some(&serde_json::json!({"attempt": 1}))
    );
    assert_eq!(decoded, call, "full value preserved with a plan stashed");
}

// ── Leg-kind / `adopted` round-trip (ADR-0014) ──────────────────────────────
//
// Port of `tests/codec/leg-kind-round-trip.test.ts`. `Leg.kind` and
// `Leg.adopted` ride in the replicated call body and must survive
// encode → decode: a media leg's `kind=Media` / `adopted=false` is what the
// unadopted-leg takeover gate keys on after a worker failover, so losing it
// across replication would mis-route the parked MRF leg on the backup.
//
// CODEC ARMS. The TS source runs each case across THREE codecs — `Msgpack`,
// `MsgpackRecords`, and `Protobuf`. The Rust `call` crate has exactly **one**
// `CallBodyCodec` impl today: `MsgpackCodec` (`rmp-serde`'s positional/array
// encoding, the analogue of the TS `Msgpack` arm). There is no Rust
// `MsgpackRecords` (the TS records mode is a second msgpack shape; not ported)
// and **no Rust protobuf codec at all** — it is explicitly deferred
// (ADR-0008, MIGRATION_STATUS "Protobuf codec + call.proto … deferred"). The
// Protobuf arm — the exact path the TS test exists to police, since protobuf
// `JSON.stringify`-of-`ext` is what would corrupt raw bytes — therefore has no
// Rust home yet. See the `protobuf_arm_*` placeholders at the end of this file.

/// `it("a-leg kind/adopted survive encode→decode")` — the explicitly-tagged
/// a-leg carries `kind=A` / `adopted=true` through the positional msgpack codec.
#[test]
fn leg_kind_a_leg_kind_adopted_survive() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    call.a_leg.kind = Some(LegKind::A);
    call.a_leg.adopted = Some(true);
    // An unadopted media leg rides in b_legs (mirrors the TS `mediaLeg`).
    let mut media = representative_call().a_leg;
    media.leg_id = "media-1".into();
    media.kind = Some(LegKind::Media);
    media.adopted = Some(false);
    call.b_legs.push(media);

    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded.a_leg.kind, Some(LegKind::A));
    assert_eq!(decoded.a_leg.adopted, Some(true));
    assert_eq!(decoded, call, "full value preserved");
}

/// `it("media leg kind=media / adopted=false survive")` — the unadopted media
/// leg's role/adopted bit round-trips intact (`false` is the gate-relevant
/// value, distinct from absent).
#[test]
fn leg_kind_media_leg_kind_adopted_survive() {
    let codec = MsgpackCodec::new();
    let mut call = representative_call();
    call.a_leg.kind = Some(LegKind::A);
    call.a_leg.adopted = Some(true);
    let mut media = representative_call().a_leg;
    media.leg_id = "media-1".into();
    media.kind = Some(LegKind::Media);
    media.adopted = Some(false);
    call.b_legs.push(media);

    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    let media = decoded
        .b_legs
        .iter()
        .find(|l| l.leg_id == "media-1")
        .expect("media leg present after round-trip");
    assert_eq!(media.kind, Some(LegKind::Media));
    assert_eq!(media.adopted, Some(false));
}

/// `it("a leg without kind/adopted decodes with both absent (legacy bodies)")`.
///
/// Rust-codec nuance vs. TS. The TS msgpack/records codecs are *named*-field
/// (a body emitted before `kind`/`adopted` existed simply lacks those keys and
/// decodes them as `undefined`). The Rust `MsgpackCodec` is **positional**:
/// `kind` / `adopted` are mid-`Leg` `Option`s with no `#[serde(default)]`, so a
/// *value* of `None` serializes as an explicit nil and round-trips to `None`
/// (asserted here — the value-level analogue of "both absent"), BUT a genuinely
/// field-*absent* wire body (one struct slot short) does **not** decode under
/// the positional codec — the next field's marker would land where the bool is
/// expected. Same back-compat caveat documented for `emergency` above; only
/// the LAST field (`sm_cursors`, `#[serde(default)]`) supports field-absent
/// decode. So this pins the **value-absent** contract, which is all the
/// positional codec can express.
#[test]
fn leg_kind_absent_decodes_absent() {
    let codec = MsgpackCodec::new();
    let mut legacy = representative_call();
    legacy.a_leg.kind = None;
    legacy.a_leg.adopted = None;

    let decoded = codec.decode(&codec.encode(&legacy)).unwrap();
    assert_eq!(decoded.a_leg.kind, None);
    assert_eq!(decoded.a_leg.adopted, None);
    assert_eq!(decoded, legacy, "full value preserved with both absent");
}

// ── Service-ext round-trip (ADR-0016) ───────────────────────────────────────
//
// Port of `tests/codec/service-ext-round-trip.test.ts`. Locks the load-bearing
// invariant: `Call.ext[id]` / `Leg.ext[id]` carry the **Encoded** (JSON-safe)
// form of a service slice, never a decoded value. A `promotedSdp` rides as its
// **base64 string**, never as raw bytes.
//
// The TS test's whole reason to exist is the protobuf `JSON.stringify`-of-`ext`
// path: a raw `Uint8Array` would corrupt into a numeric-keyed object, so the
// base64 string is the at-rest form that survives. In Rust, `ExtMap` values are
// already `serde_json::Value`, and the at-rest slice stores `promotedSdp` /
// `cInitialSdp` as `Value::String(<base64>)`. These cases assert the base64
// string survives encode→decode **as a string** through the positional msgpack
// codec AND re-decodes (base64 → bytes) to the original SDP bytes — the same
// contract, on the codec arm that exists. The protobuf arm (where the
// JSON.stringify corruption would actually bite) is deferred — see the
// `protobuf_arm_*` placeholders below.

/// The PEM promote service's SDP — base64 is the at-rest form. Mirrors the TS
/// `sdpBytes`.
const PEM_SDP: &[u8] =
    b"v=0\r\no=- 200 1 IN IP4 10.20.0.1\r\ns=-\r\nc=IN IP4 10.20.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\n";
/// The transfer C-leg initial SDP — same Uint8Array/base64 trap as PEM's
/// `promotedSdp`. Mirrors the TS `cInitialSdpBytes`.
const TRANSFER_C_SDP: &[u8] =
    b"v=0\r\no=- 9001 1 IN IP4 10.30.0.7\r\ns=-\r\nc=IN IP4 10.30.0.7\r\nt=0 0\r\nm=audio 50000 RTP/AVP 8\r\n";

/// The encoded `promote-pem` call-ext slice: `promotedSdp` as a base64 *string*
/// (the at-rest form), the rest as plain JSON. Mirrors the TS `encodedSlice`.
fn pem_call_ext_encoded() -> serde_json::Value {
    serde_json::json!({
        "promoted": true,
        "promotedSdp": BASE64.encode(PEM_SDP),
        "windowOpen": true,
        "resyncReinviteCSeq": 42,
    })
}

/// The encoded `transfer` call-ext slice: `cInitialSdp` as a base64 *string*.
/// Mirrors the TS `encodedTransferSlice`.
fn transfer_call_ext_encoded() -> serde_json::Value {
    serde_json::json!({
        "phase": "c-realigning",
        "referrerLegId": "b-1",
        "referToUri": "sip:carol@example.com",
        "effectiveReferToUri": "sip:carol@10.30.0.7",
        "callbackContext": "xfer-ctx-77",
        "cLegId": "b-2",
        "referCSeq": 7,
        "startedAtMs": 1_779_440_099_000_i64,
        "lastCLegNotifiedStatus": 180,
        "cInitialSdp": BASE64.encode(TRANSFER_C_SDP),
    })
}

/// A `Call` carrying both service call-ext slices plus a typed leg-ext entry on
/// the a-leg. Mirrors the TS `callWithExt`.
fn call_with_ext() -> call::Call {
    let mut call = representative_call();
    let ext = call.ext.get_or_insert_with(Default::default);
    ext.insert("promote-pem".into(), pem_call_ext_encoded());
    ext.insert("transfer".into(), transfer_call_ext_encoded());
    // The transfer service addresses legs by id (no leg-ext); a synthetic
    // `demo-leg-service` exercises the generic leg-ext capability on the a-leg.
    call.a_leg
        .ext
        .get_or_insert_with(Default::default)
        .insert("demo-leg-service".into(), serde_json::json!({ "role": "media" }));
    call
}

/// `it("call-ext promotedSdp base64 survives encode→decode and re-decodes to
/// the original bytes")`.
#[test]
fn service_ext_pem_promoted_sdp_base64_survives() {
    let codec = MsgpackCodec::new();
    let call = call_with_ext();
    let decoded = codec.decode(&codec.encode(&call)).unwrap();

    let slice = decoded
        .ext
        .as_ref()
        .and_then(|e| e.get("promote-pem"))
        .expect("promote-pem call-ext present");
    assert_eq!(slice, &pem_call_ext_encoded(), "slice survives verbatim");
    // The JSON path only ever sees the base64 string, never raw bytes.
    assert!(slice["promotedSdp"].is_string(), "promotedSdp is a base64 string at rest, not bytes");
    // Re-decoding the slice yields the original bytes.
    let re_decoded = BASE64.decode(slice["promotedSdp"].as_str().unwrap()).expect("base64 decodes");
    assert_eq!(re_decoded, PEM_SDP, "re-decodes to the original SDP bytes");
    assert_eq!(slice["resyncReinviteCSeq"], 42);
}

/// `it("leg-ext entry survives encode→decode")`.
#[test]
fn service_ext_leg_ext_entry_survives() {
    let codec = MsgpackCodec::new();
    let call = call_with_ext();
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(
        decoded.a_leg.ext.as_ref().and_then(|e| e.get("demo-leg-service")),
        Some(&serde_json::json!({ "role": "media" }))
    );
}

/// `it("transfer call-ext cInitialSdp base64 survives and re-decodes to
/// bytes")`.
#[test]
fn service_ext_transfer_c_initial_sdp_base64_survives() {
    let codec = MsgpackCodec::new();
    let call = call_with_ext();
    let decoded = codec.decode(&codec.encode(&call)).unwrap();

    let slice =
        decoded.ext.as_ref().and_then(|e| e.get("transfer")).expect("transfer call-ext present");
    assert_eq!(slice, &transfer_call_ext_encoded(), "slice survives verbatim");
    // cInitialSdp is a base64 string at rest, never raw bytes.
    assert!(slice["cInitialSdp"].is_string(), "cInitialSdp is a base64 string at rest, not bytes");
    let re_decoded = BASE64.decode(slice["cInitialSdp"].as_str().unwrap()).expect("base64 decodes");
    assert_eq!(re_decoded, TRANSFER_C_SDP, "re-decodes to the original bytes");
    assert_eq!(slice["phase"], "c-realigning");
    assert_eq!(slice["cLegId"], "b-2");
}

// ── Protobuf arm — DEFERRED (dependency absent) ─────────────────────────────
//
// TODO(migration): port the `Protobuf` describe-blocks of BOTH
// `leg-kind-round-trip.test.ts` and `service-ext-round-trip.test.ts` once a
// Rust protobuf `CallBodyCodec` exists.
//
// This migration item ("Protobuf-arm of leg-kind & service-ext round-trip
// contracts") was scheduled to STACK ON a prior item delivering the "Protobuf
// CallBodyCodec impl + field-mapping shims (toProtoObject/fromProtoObject,
// *IsNull/*Present/*Json)". That dependency is **not present** on the branch
// this work stacks on (migration/23) — and is unported anywhere in the Rust
// workspace:
//   - `crates/call/src/codec.rs` / `lib.rs` state the protobuf codec is
//     deferred; `MsgpackCodec` is the only impl behind the `CallBodyCodec`
//     trait.
//   - `MIGRATION_STATUS.md` lists "Protobuf codec + call.proto — needs a
//     prost/build.rs toolchain + field-mapping … deferred".
//   - `grep -rE 'prost|protobuf|ProtobufCodec'` over `crates/` is empty.
//
// Porting it from scratch (TS source: `src/call/codec/{protobuf.ts (341 LOC),
// call.proto (205 LOC), call.proto.gen.cjs}`) means a `call.proto` + prost
// `build.rs` toolchain + the ~30 `*IsNull` / `*Present` / `*Json` field-mapping
// shims — a separate migration item, not this one. The Protobuf arm is the
// path this contract exists to police (protobuf `JSON.stringify`-of-`ext` is
// what would corrupt a raw `Uint8Array`/`Vec<u8>` into a numeric-keyed object),
// so it MUST be ported when the codec lands — the msgpack arm above does not
// exercise that corruption path (rmp-serde carries the `serde_json::Value`
// string natively). When the protobuf codec exists, mirror every
// `leg_kind_*` / `service_ext_*` case above against it (drive both arms from a
// shared `&[&dyn CallBodyCodec]` table, the analogue of the TS `CODECS` loop).

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// P1 — round-trip preserves the value (covers P5/P6/P8: Option/None and
    /// empty collections are part of the generated space).
    #[test]
    fn p1_round_trip(call in arb_call()) {
        let codec = MsgpackCodec::new();
        let decoded = codec.decode(&codec.encode(&call)).unwrap();
        prop_assert_eq!(decoded, call);
    }

    /// P2 — encode is deterministic (relies on `BTreeMap` for `ext`/headers).
    #[test]
    fn p2_encode_deterministic(call in arb_call()) {
        let codec = MsgpackCodec::new();
        prop_assert_eq!(codec.encode(&call), codec.encode(&call));
    }

    /// P3 — decode is deterministic.
    #[test]
    fn p3_decode_deterministic(call in arb_call()) {
        let codec = MsgpackCodec::new();
        let bytes = codec.encode(&call);
        prop_assert_eq!(codec.decode(&bytes).unwrap(), codec.decode(&bytes).unwrap());
    }

    /// P14 — encode never returns an empty buffer.
    #[test]
    fn p14_non_empty_output(call in arb_call()) {
        let codec = MsgpackCodec::new();
        prop_assert!(!codec.encode(&call).is_empty());
    }
}
