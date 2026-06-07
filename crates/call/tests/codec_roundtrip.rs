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
