//! Protobuf `CallBodyCodec` round-trip + property suite — the Rust port of the
//! **`ProtobufLayer`** cases from `tests/codec/round-trip-property.test.ts`
//! (`describe 'CallBodyCodec contracts — property-based' > 'ProtobufLayer'`) plus
//! the two `paranoidInputs` Protobuf cases (PA1/PA2).
//!
//! Ported, by the TS `it`/`describe` name → Rust test fn:
//!
//! - `'ProtobufLayer' > P1 round-trip preserves the representative fixture` →
//!   [`p1_round_trips_representative_fixture`]
//! - `'ProtobufLayer' > P1-P14 over a 200-sample fixture pool` →
//!   [`p1_p14_over_200_sample_fixture_pool`] (proptest, 200 cases)
//! - `'ProtobufLayer' > P7 Uint8Array integrity at size 0 / 1 / 1k / 64k` →
//!   [`p7_byte_integrity_sizes`]
//! - `'ProtobufLayer' > P5 undefined preservation — callbackContext` →
//!   [`p5_callback_context_none_preserved`]
//! - `paranoidInputs > PA2 — empty Buffer decode throws synchronously
//!   (Protobuf)` → [`pa2_empty_buffer_decode_is_error`]
//! - `paranoidInputs > PA1 — Schema.is(CallSchema) rejects a malformed encode
//!   input (Protobuf)` → [`pa1_malformed_encode_input`] (see its doc for why the
//!   Rust shape differs)
//!
//! ## Why no clock / why this is a default-lane test
//!
//! These are pure, synchronous value tests: no `tokio`, no `Clock`, no timers, no
//! `Harness`. The CLAUDE.md timer/clock hazards (epoch/`Key`, transit ≥ 1 ms,
//! advance discipline, per-call state release) and the >60 s / fake-clock
//! test-runtime policy therefore do not apply — the whole file runs in well under
//! a second on the real clock (the TS suite ran these under `it.effect`'s
//! TestClock only because every test in that file shares the wrapper, not because
//! the codec needs a clock). Same posture as the sibling `proto_codegen.rs` and
//! `codec_roundtrip.rs`.
//!
//! ## Integer domain (the one place the port constrains the input)
//!
//! `proto/call.proto` mirrors the JS number domain: cseqs / ports / counts are
//! `int32`, timestamps are `double`. The Rust model uses `i64` (and `u16` ports),
//! so the codec narrows on the wire. The shared `arb_call()` generator (built for
//! the msgpack codec) draws full-range `i64`s, which `int32` cannot hold. The TS
//! `ProtobufLayer` pool never hit this because `buildFixturePool` emits realistic
//! SIP values that already sit inside `int32`. We reproduce that precondition
//! exactly with [`normalize_for_proto`], which pushes each generated value
//! through the *same* `as i32` / `as f64` narrowing the codec applies — making
//! the input a fixed point of the wire domain, so round-trip equality holds for
//! any drawn magnitude. (This is the honest analogue of "the fixture pool only
//! contains representable values," not a workaround for a codec bug.) The same
//! helper also drops `sm_cursors`, which the protobuf schema (branch 29) has no
//! field for — see [`normalize_for_proto`].

mod common;

use call::{Call, CallBodyCodec, CallDecodeError, PolicyUpdateBody, ProtobufCodec};
use common::{arb_call, representative_call};
use proptest::prelude::*;

// ── Input normalisation to the proto int32 / double wire domain ──────────────

/// Narrow an `i64` through `int32`, exactly as the codec does on the wire.
fn i32_fixed(n: i64) -> i64 {
    n as i32 as i64
}
/// Narrow an `Option<i64>` field that maps to a proto `int32`.
fn opt_i32_fixed(n: Option<i64>) -> Option<i64> {
    n.map(i32_fixed)
}
/// Narrow an `i64` epoch/seconds value through `double`, as the codec does.
fn f64_fixed(n: i64) -> i64 {
    n as f64 as i64
}

/// Push every integer/timestamp field of a generated [`Call`] into the proto
/// wire domain (`int32` for cseqs/ports/counts, `double` for timestamps), so the
/// value is a fixed point under the codec's narrowing and round-trips exactly.
/// Ports are `u16` and always fit `int32`, so they need no adjustment.
///
/// It also resets the two model fields the **branch-29 proto schema cannot
/// carry** (it mirrors the older TS `Call` shape) to their wire-absent default,
/// so the round-trip compares only what the schema actually represents:
///
/// - [`Call::sm_cursors`] (cleared) — the ADR-0016 state-machine cursors. The
///   proto predates the typed-ext slices; the codec's `from_proto` reconstructs
///   them empty. The shared `arb_call()` (built for the msgpack codec, which
///   *does* carry them) populates this; the other three typed-ext fields
///   (`transfer`/`relay_first_18x`/`promote_pem`) are already `None` from
///   `arb_call()`.
/// - `topology.bak_gen` (zeroed) — the `(p,b)` version-vector **backup** counter
///   (ADR-0014). The TS `CallTopology` is `{pri, bak, gen}`, so the proto has no
///   `bakGen` field and the codec resets `b = 0` on decode. This is the one carry
///   loss with a correctness consequence if protobuf were ever the replication
///   codec (it is not — msgpack is); see `ProtobufCodec`/`from_proto`'s doc.
fn normalize_for_proto(mut c: Call) -> Call {
    fn fix_leg(l: &mut call::Leg) {
        for d in &mut l.dialogs {
            d.sip.local_cseq = i32_fixed(d.sip.local_cseq);
            d.ext.remote_cseq = opt_i32_fixed(d.ext.remote_cseq);
            for p in &mut d.ext.inbound_pending_requests {
                p.outbound_cseq = i32_fixed(p.outbound_cseq);
                p.inbound_cseq = i32_fixed(p.inbound_cseq);
            }
        }
        // `noAnswerTimeoutSec` rides a proto `double`.
        l.no_answer_timeout_sec = l.no_answer_timeout_sec.map(f64_fixed);
    }

    fix_leg(&mut c.a_leg);
    for l in &mut c.b_legs {
        fix_leg(l);
    }

    c.created_at = f64_fixed(c.created_at);
    c.a_leg_pending_cseq = opt_i32_fixed(c.a_leg_pending_cseq);
    c.worker_index = opt_i32_fixed(c.worker_index);
    c.message_count = opt_i32_fixed(c.message_count);
    if let Some(t) = &mut c.topology {
        t.gen = i32_fixed(t.gen);
        // No `bakGen` in the proto schema (TS `CallTopology` is `{pri,bak,gen}`).
        t.bak_gen = 0;
    }
    for e in &mut c.limiter_entries {
        e.limit = i32_fixed(e.limit);
        e.origin_window = f64_fixed(e.origin_window);
    }
    for t in &mut c.timers {
        t.fire_at = f64_fixed(t.fire_at);
    }
    for e in &mut c.cdr_events {
        e.timestamp = f64_fixed(e.timestamp);
        e.status_code = opt_i32_fixed(e.status_code);
    }

    // Out of the protobuf schema's domain (see this fn's doc) — drop so the
    // round-trip compares only fields the wire schema actually carries.
    c.sm_cursors.clear();
    c
}

// ── P1: representative fixture ───────────────────────────────────────────────

/// `ProtobufLayer` > "P1 round-trip preserves the representative fixture".
/// The TS test spot-checks `callRef` / `aLeg.callId` / `bLegs.length`; the Rust
/// model derives `PartialEq`, so we assert the *whole* value is preserved (a
/// strictly stronger P1) on top of the same spot-checks.
#[test]
fn p1_round_trips_representative_fixture() {
    let codec = ProtobufCodec::new();
    let call = representative_call();
    let encoded = codec.encode(&call);
    assert!(!encoded.is_empty(), "P14: non-empty output");
    let decoded = codec.decode(&encoded).expect("decode");

    // The TS spot-checks, verbatim.
    assert_eq!(decoded.call_ref, call.call_ref);
    assert_eq!(decoded.a_leg.call_id, call.a_leg.call_id);
    assert_eq!(decoded.b_legs.len(), call.b_legs.len());

    // Stronger than TS: the representative fixture is already in the proto wire
    // domain (small cseqs, epoch-ms timestamps < 2^53), so the full value
    // round-trips.
    assert_eq!(
        decoded,
        call,
        "P1: protobuf round-trip preserves the whole representative fixture"
    );
}

// ── P7: byte integrity at 0 / 1 / 1k / 64k ───────────────────────────────────

/// `ProtobufLayer` > "P7 Uint8Array integrity at size 0 / 1 / 1k / 64k".
/// Exercised on `aLegInvite.body` (proto `bytes`), exactly as the TS case.
#[test]
fn p7_byte_integrity_sizes() {
    let codec = ProtobufCodec::new();
    let mut call = representative_call();
    for len in [0usize, 1, 1024, 65536] {
        let body: Vec<u8> = (0..len).map(|i| (i & 0xff) as u8).collect();
        call.a_leg_invite.body = body.clone();
        let decoded = codec.decode(&codec.encode(&call)).unwrap();
        assert_eq!(decoded.a_leg_invite.body, body, "len={len}");
    }
}

// ── P5: undefined preservation (callbackContext) ─────────────────────────────

/// `ProtobufLayer` > "P5 undefined preservation — callbackContext". The TS case
/// sets `callbackContext: undefined` and asserts it comes back undefined; the
/// Rust analogue is `None`, carried by proto3 absence.
#[test]
fn p5_callback_context_none_preserved() {
    let codec = ProtobufCodec::new();
    let mut call = representative_call();
    call.callback_context = None;
    let decoded = codec.decode(&codec.encode(&call)).unwrap();
    assert_eq!(decoded.callback_context, None);
    assert_eq!(decoded, call, "P5: clearing callbackContext leaves the rest intact");
}

/// Companion to P5 that pins the side-channels the protobuf codec exists to
/// carry (the schema's central claim: proto3 collapses absent/null/empty so an
/// explicit bit restores the distinction). Not a distinct TS `it`, but it is the
/// `ProtobufLayer`-specific behaviour the TS pool covers statistically via the
/// fixture mixer; here we pin it directly on the representative fixture.
#[test]
fn null_and_presence_distinctions_round_trip() {
    let codec = ProtobufCodec::new();
    let rt = |c: &Call| codec.decode(&codec.encode(c)).unwrap();

    // `activePeer` — `Some` vs `None` (the source's value vs `null`).
    let mut call = representative_call();
    call.active_peer = None;
    assert_eq!(rt(&call).active_peer, None, "activePeer None survives");

    // `aLegPendingVias` — the `Option<Vec>` three-state: absent vs empty vs
    // populated (the `*Present` bit is what makes empty distinguishable).
    for v in [None, Some(vec![]), Some(vec!["SIP/2.0/UDP h;branch=z9hG4bK1".to_string()])] {
        let mut c = representative_call();
        c.a_leg_pending_vias = v.clone();
        assert_eq!(rt(&c).a_leg_pending_vias, v, "aLegPendingVias {v:?} survives");
    }

    // `remoteCSeq` on a dialog ext — `None` (the source's `null`) vs a value.
    for cseq in [None, Some(0i64), Some(99)] {
        let mut c = representative_call();
        c.a_leg.dialogs[0].ext.remote_cseq = cseq;
        assert_eq!(
            rt(&c).a_leg.dialogs[0].ext.remote_cseq,
            cseq,
            "remoteCSeq {cseq:?} survives"
        );
    }

    // `policyUpdateBody` — the three-way union: None / force-empty / bytes.
    for body in [
        None,
        Some(PolicyUpdateBody::Empty),
        Some(PolicyUpdateBody::Bytes(vec![1, 2, 3, 4])),
    ] {
        let mut c = representative_call();
        c.policy_update_body = body.clone();
        assert_eq!(rt(&c).policy_update_body, body, "policyUpdateBody {body:?} survives");
    }

    // `activeRules` — `Option<Vec>` absent vs empty vs populated.
    for rules in [
        None,
        Some(vec![]),
        Some(vec![call::ActiveRule { id: "r".into(), active: true }]),
    ] {
        let mut c = representative_call();
        c.active_rules = rules.clone();
        assert_eq!(rt(&c).active_rules, rules, "activeRules {rules:?} survives");
    }
}

// ── PA2 / PA1: paranoid inputs ───────────────────────────────────────────────

/// `paranoidInputs` > "PA2 — empty Buffer decode throws synchronously
/// (Protobuf)". The TS wrapper raises `ParanoidInputViolation`; the Rust codec
/// surfaces the same precondition as the typed [`CallDecodeError::Empty`]
/// (synchronous, by `Result`, before any wire parse).
#[test]
fn pa2_empty_buffer_decode_is_error() {
    let codec = ProtobufCodec::new();
    assert!(matches!(codec.decode(&[]), Err(CallDecodeError::Empty)));
}

/// `paranoidInputs` > "PA1 — Schema.is(CallSchema) rejects a malformed encode
/// input (Protobuf)".
///
/// In TS the input is an *untyped* JS object (`{ ...call, callRef: 42 }`) and the
/// `paranoidInputs` wrapper runs `Schema.is(CallSchema)` to reject it before
/// encoding. In Rust the encode signature is `encode(&Call)`, so a `callRef: 42`
/// is **unrepresentable** — the type system is the schema check, and the PA1
/// guard collapses into a compile-time guarantee (the same way ADR-0008 notes
/// P10/P11/P13 collapse for the typed codec). There is therefore no runtime
/// malformed-encode path to assert; the corresponding wire-level guard the Rust
/// codec *does* own is **malformed-decode** rejection, which we assert here as
/// the faithful analogue: a non-proto / truncated body is a typed
/// [`CallDecodeError::Decode`], not a panic and not a silent default `Call`.
#[test]
fn pa1_malformed_encode_input() {
    let codec = ProtobufCodec::new();

    // Garbage that is not a valid proto3 frame (a lone continuation byte → an
    // unterminated varint) must be a typed decode error, surfaced synchronously.
    let garbage = [0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(
        matches!(codec.decode(&garbage), Err(CallDecodeError::Decode(_))),
        "a malformed proto frame must be a typed Decode error"
    );

    // A *well-formed but structurally-invalid* body — valid proto3 wire bytes
    // that decode to a `Call` missing the required `aLeg` submessage — must also
    // be rejected by `from_proto` (not produce a half-built `Call`). We get such
    // bytes by encoding only a `callRef` string at tag 1.
    let mut only_call_ref = Vec::new();
    only_call_ref.push(1u8 << 3 | 2); // tag 1, wire type 2 (length-delimited)
    only_call_ref.push(3); // len
    only_call_ref.extend_from_slice(b"xyz");
    assert!(
        matches!(codec.decode(&only_call_ref), Err(CallDecodeError::Decode(_))),
        "a proto body without the required aLeg must be a typed Decode error"
    );
}

// ── P1-P14 over a 200-sample fixture pool ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// `ProtobufLayer` > "P1-P14 over a 200-sample fixture pool (paranoid +
    /// property)". The TS test walks a 200-element pool asserting non-empty
    /// encode (P14) + `callRef` preserved on decode. The Rust analogue draws 200
    /// proptest cases from the same fixture mixer (`arb_call`), normalised into
    /// the proto wire domain (see the module header), and asserts the **whole**
    /// value round-trips (P1, stronger than the TS `callRef`-only check) on top of
    /// non-empty encode (P14) and `callRef` preservation.
    #[test]
    fn p1_p14_over_200_sample_fixture_pool(call in arb_call().prop_map(normalize_for_proto)) {
        let codec = ProtobufCodec::new();
        let encoded = codec.encode(&call);
        prop_assert!(!encoded.is_empty(), "P14: non-empty output");
        let decoded = codec.decode(&encoded).expect("decode");
        prop_assert_eq!(&decoded.call_ref, &call.call_ref, "callRef preserved");
        prop_assert_eq!(decoded, call, "P1: whole value preserved");
    }

    /// P2/P3 analogue for the protobuf codec: encode is deterministic and decode
    /// is deterministic (prost field order is the tag order). Covered statistically
    /// by the TS pool; pinned directly here.
    #[test]
    fn encode_and_decode_are_deterministic(call in arb_call().prop_map(normalize_for_proto)) {
        let codec = ProtobufCodec::new();
        let a = codec.encode(&call);
        let b = codec.encode(&call);
        prop_assert_eq!(&a, &b, "P2: encode deterministic");
        let da = codec.decode(&a).unwrap();
        let db = codec.decode(&b).unwrap();
        prop_assert_eq!(da, db, "P3: decode deterministic");
    }
}
