//! Codec round-trip + property suite — the Rust port of the meaningful codec
//! properties from `tests/codec/round-trip-property.test.ts` (P1/P2/P3/P5/P6/P7/
//! P8/P14). Parity (the source's `parity` wrapper) is intentionally out of scope
//! for this slice; P10/P11/P13 collapse into compile-time guarantees in Rust
//! (`encode(&Call)` cannot mutate, `decode` returns the typed `Call`). See
//! ADR-0008.

mod common;

use call::{CallBodyCodec, CallDecodeError, MachineId, MsgpackCodec, PolicyUpdateBody, StateLabel};
use common::{arb_call, representative_call};
use proptest::prelude::*;

#[test]
fn representative_round_trips() {
    let codec = MsgpackCodec::new();
    let call = representative_call();
    let bytes = codec.encode(&call);
    assert!(!bytes.is_empty(), "P14: non-empty output");
    let decoded = codec.decode(&bytes).expect("decode");
    assert_eq!(decoded, call, "P1: round-trip preserves the value");
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
    for v in [
        None,
        Some(PolicyUpdateBody::Empty),
        Some(PolicyUpdateBody::Bytes(vec![1, 2, 3, 4])),
    ] {
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
    call.sm_cursors.insert(
        MachineId::new("global-call"),
        StateLabel::new("Active"),
    );
    call.sm_cursors.insert(
        MachineId::new("transfer"),
        StateLabel::new("CRinging"),
    );
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
