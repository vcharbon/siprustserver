//! Codec round-trip + property suite — the Rust port of the meaningful codec
//! properties from `tests/codec/round-trip-property.test.ts` (P1/P2/P3/P5/P6/P7/
//! P8/P14). Parity (the source's `parity` wrapper) is intentionally out of scope
//! for this slice; P10/P11/P13 collapse into compile-time guarantees in Rust
//! (`encode(&Call)` cannot mutate, `decode` returns the typed `Call`). See
//! ADR-0008.

mod common;

use call::{CallBodyCodec, CallDecodeError, MsgpackCodec, PolicyUpdateBody};
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
