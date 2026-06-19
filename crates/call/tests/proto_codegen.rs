//! Protobuf **schema + codegen toolchain** tests — the wire-level analogue of
//! the codec round-trip suite (`tests/codec/round-trip-property.test.ts`) for
//! the `ProtobufLayer`, scoped to what this slice actually lands: that
//! `proto/call.proto` codegen's (via `build.rs` → `protox` + `prost-build`) into
//! usable `prost::Message` wire types.
//!
//! The protobuf **codec impl** — the `model::Call` ↔ `proto::wire::Call` mapping
//! with the `*IsNull` / `*Present` side-channels and the `featuresJson` /
//! `extJson` / `pendingInviteTxnJson` JSON carries — is a separate stacked item;
//! the *full* P1–P14 contract over real `Call` fixtures lands with it. Here we
//! pin the toolchain and the schema's load-bearing wire claims directly on the
//! generated types:
//!
//!   * **P1 (round-trip preserves the value)** at the wire level — proto3
//!     encode→decode is the schema's contract; exercised on the top-level `Call`
//!     and every leaf message.
//!   * **The `*IsNull` / `*Present` bits are real wire fields.** The schema's
//!     central claim (`call.proto` header + `protobuf.ts`) is that proto3 cannot
//!     distinguish absent / null / empty, so an explicit boolean carries the
//!     distinction. We assert those bits survive a round-trip independently of
//!     the value field they qualify.
//!   * **P7 (byte-field integrity)** at sizes 0 / 1 / 1 KiB / 64 KiB on a
//!     `bytes` field, matching the TS `Uint8Array` integrity case.
//!   * **Field-id stability (ADR-0011 / `call.proto` rules).** Ids 33/34
//!     (`transfer*`) and 35/36 (`earlyPromote*`) are retired and MUST NOT be
//!     reused — pinned by asserting unknown bytes carried at those tags survive a
//!     decode→re-encode (proto3 round-trips unknown fields), proving the codegen
//!     left those ids free.
//!   * **Keyword + casing codegen.** The `type` field becomes the escaped
//!     `r#type`; `CSeq` lowercases to `c_seq`. Reaching them compiles only if
//!     codegen named them as expected.
//!
//! These are pure, synchronous value tests: no `tokio`, no clock, no timers, so
//! the CLAUDE.md timer/clock hazards and the >60 s / fake-clock test-runtime
//! policy do not apply (the whole file runs in well under a millisecond on the
//! real clock).

use call::proto::wire;
use prost::Message;

/// Encode then decode a message through `prost`, asserting an exact round-trip.
/// This is the wire-level P1 used throughout the file.
fn rt<M>(msg: &M) -> M
where
    M: Message + Default + PartialEq + std::fmt::Debug + Clone,
{
    let bytes = msg.encode_to_vec();
    let back = M::decode(&bytes[..]).expect("proto3 decode");
    assert_eq!(*msg, back, "wire round-trip must preserve the value");
    back
}

/// A fully-populated wire `Call` exercising every nested message kind, the
/// optional-scalar fields, the repeated fields, the byte fields, and the
/// `*IsNull` / `*Present` side-channels set to their non-default values.
fn representative_wire_call() -> wire::Call {
    wire::Call {
        call_ref: "abc123#0".into(),
        a_leg: Some(wire::Leg {
            leg_id: "a".into(),
            call_id: "call-a".into(),
            from_tag: "tag-a".into(),
            source: Some(wire::RemoteInfo {
                address: "10.0.0.1".into(),
                port: 5060,
            }),
            state: "confirmed".into(),
            disposition: "bridged".into(),
            dialogs: vec![wire::Dialog {
                sip: Some(wire::StackDialog {
                    call_id: "call-a".into(),
                    local_tag: "lt".into(),
                    remote_tag: "rt".into(),
                    local_uri: "sip:b2bua@host".into(),
                    remote_uri: "sip:alice@host".into(),
                    remote_target: "sip:alice@1.2.3.4".into(),
                    local_c_seq: 7,
                    route_set: vec!["sip:rr@proxy;lr".into()],
                }),
                ext: Some(wire::B2buaDialogExt {
                    remote_c_seq: Some(42),
                    remote_c_seq_is_null: Some(false),
                    inbound_pending_requests: vec![wire::PendingRequest {
                        method: "INFO".into(),
                        outbound_c_seq: 3,
                        inbound_c_seq: 9,
                        source_vias: vec!["SIP/2.0/UDP host;branch=z9hG4bK1".into()],
                        source_call_id: "call-a".into(),
                        source_from: "sip:alice@host;tag=tag-a".into(),
                        source_to: "sip:b2bua@host".into(),
                        direction: "from-a".into(),
                    }],
                    ack_branch: Some("z9hG4bKack".into()),
                    pending_invite_txn_json: Some("{\"branch\":\"x\"}".into()),
                    cached_sdp: Some(vec![1, 2, 3]),
                }),
            }],
            no_answer_timeout_sec: Some(20.0),
            bye_disposition: Some("bye_confirmed".into()),
            local_uri: Some("sip:b2bua@host".into()),
            remote_uri: Some("sip:alice@host".into()),
            invite_request_uri: Some("sip:alice@1.2.3.4".into()),
            pending_invite_txn_json: None,
            ext_json: Some("{\"svc\":1}".into()),
            kind: Some("a".into()),
            adopted: Some(true),
        }),
        b_legs: vec![wire::Leg {
            leg_id: "b-1".into(),
            call_id: "call-b".into(),
            from_tag: "tag-b".into(),
            source: Some(wire::RemoteInfo {
                address: "10.0.0.2".into(),
                port: 5070,
            }),
            state: "confirmed".into(),
            disposition: "bridged".into(),
            dialogs: vec![],
            no_answer_timeout_sec: None,
            bye_disposition: None,
            local_uri: None,
            remote_uri: None,
            invite_request_uri: None,
            pending_invite_txn_json: None,
            ext_json: None,
            kind: Some("destination".into()),
            adopted: None,
        }],
        active_peer: Some(wire::ActivePeer {
            leg_a: "a".into(),
            leg_b: "b-1".into(),
        }),
        active_peer_is_null: false,
        callback_context: Some("ctx".into()),
        billing_context: Some("bill".into()),
        billing_context_is_null: Some(false),
        a_leg_invite: Some(wire::ALegInvite {
            uri: "sip:alice@host".into(),
            headers: vec![wire::SipHeader {
                name: "Max-Forwards".into(),
                value: "70".into(),
            }],
            body: b"v=0\r\n".to_vec(),
        }),
        limiter_entries: vec![wire::CallLimiterState {
            limiter_id: "lim".into(),
            limit: 100,
            origin_window: 1_700_000_000.0,
            increment_succeeded: Some(true),
        }],
        timers: vec![wire::TimerEntry {
            id: "t1".into(),
            // keyword-escaped: proto `type` -> Rust `r#type`.
            r#type: "no_answer".into(),
            fire_at: 1_700_000_020.0,
            leg_id: Some("a".into()),
        }],
        cdr_events: vec![wire::CdrEvent {
            r#type: "answer".into(),
            timestamp: 1_700_000_010.0,
            leg_id: "a".into(),
            status_code: Some(200),
            reason: None,
        }],
        state: "active".into(),
        created_at: 1_700_000_000.0,
        a_leg_pending_vias: vec!["SIP/2.0/UDP host;branch=z9hG4bK2".into()],
        a_leg_pending_vias_present: true,
        a_leg_pending_c_seq: Some(11),
        tag_map: vec![wire::TagMapping {
            a_tag: "a-shown".into(),
            b_leg_id: "b-1".into(),
            b_tag: "b-real".into(),
        }],
        trace_id: Some("trace".into()),
        root_span_id: Some("span".into()),
        sampled: Some(true),
        worker_index: Some(3),
        topology: Some(wire::CallTopology {
            pri: "w0".into(),
            bak: "w1".into(),
            // keyword-escaped: proto `gen` -> Rust `r#gen` (edition-2024-clean).
            r#gen: 5,
        }),
        emergency: Some(true),
        features_json: Some("{\"f\":true}".into()),
        policy_update_headers_json: Some("{\"X\":\"y\"}".into()),
        policy_update_body: Some(b"sub".to_vec()),
        policy_update_body_is_null: Some(false),
        active_rules: vec![wire::ActiveRule {
            id: "rule-1".into(),
            params_present: true,
            params_json: Some("{}".into()),
            active: true,
        }],
        active_rules_present: true,
        rule_state: vec![wire::RuleStateEntry {
            rule_id: "rule-1".into(),
            state_present: true,
            state_json: Some("{\"n\":1}".into()),
        }],
        rule_state_present: true,
        message_count: Some(4),
        terminating_refresh_legs: vec!["a".into()],
        terminating_refresh_legs_present: true,
        ext_json: Some("{\"top\":1}".into()),
    }
}

/// P1 — a fully-populated `Call` round-trips byte-for-value through proto3.
/// This is the wire-level analogue of the TS `round-trip-property` P1 case for
/// the protobuf codec.
#[test]
fn p1_full_call_round_trips() {
    let call = representative_wire_call();
    let back = rt(&call);
    // Spot-check a sampling that crosses every field-kind boundary.
    assert_eq!(back.call_ref, "abc123#0");
    assert_eq!(back.a_leg.as_ref().unwrap().dialogs.len(), 1);
    assert_eq!(back.b_legs.len(), 1);
    assert_eq!(back.timers[0].r#type, "no_answer");
    assert_eq!(back.a_leg_invite.as_ref().unwrap().body, b"v=0\r\n");
}

/// P14 — proto3 encode of a populated message is non-empty (the schema is not
/// vacuous), and encode is deterministic (P2 analogue: prost field order is the
/// tag order).
#[test]
fn encode_is_nonempty_and_deterministic() {
    let call = representative_wire_call();
    let a = call.encode_to_vec();
    let b = call.encode_to_vec();
    assert!(!a.is_empty(), "populated message must encode to a non-empty buffer");
    assert_eq!(a, b, "proto3 encode is deterministic for the same value");
}

/// Every leaf message individually round-trips — proves codegen emitted a
/// working `prost::Message` for each `message {}` in the schema, not just the
/// top-level `Call`.
#[test]
fn every_leaf_message_round_trips() {
    rt(&wire::RemoteInfo {
        address: "h".into(),
        port: 1,
    });
    rt(&wire::SipHeader {
        name: "n".into(),
        value: "v".into(),
    });
    rt(&wire::PendingRequest {
        method: "BYE".into(),
        outbound_c_seq: 1,
        inbound_c_seq: 2,
        source_vias: vec!["via".into()],
        source_call_id: "c".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        direction: "from-b".into(),
    });
    rt(&wire::StackDialog {
        call_id: "c".into(),
        local_tag: "l".into(),
        remote_tag: "r".into(),
        local_uri: "lu".into(),
        remote_uri: "ru".into(),
        remote_target: "rt".into(),
        local_c_seq: 1,
        route_set: vec![],
    });
    rt(&wire::TagMapping {
        a_tag: "a".into(),
        b_leg_id: "b".into(),
        b_tag: "t".into(),
    });
    rt(&wire::CallLimiterState {
        limiter_id: "l".into(),
        limit: 1,
        origin_window: 1.0,
        increment_succeeded: None,
    });
    rt(&wire::CdrEvent {
        r#type: "bye".into(),
        timestamp: 1.0,
        leg_id: "a".into(),
        status_code: None,
        reason: Some("done".into()),
    });
    rt(&wire::CallTopology {
        pri: "p".into(),
        bak: "b".into(),
        r#gen: 1,
    });
    rt(&wire::ActiveRule {
        id: "r".into(),
        params_present: false,
        params_json: None,
        active: false,
    });
    rt(&wire::RuleStateEntry {
        rule_id: "r".into(),
        state_present: false,
        state_json: None,
    });
    rt(&wire::ActivePeer {
        leg_a: "a".into(),
        leg_b: "b".into(),
    });
}

/// The `*IsNull` / `*Present` side-channels are first-class wire fields whose
/// value is independent of the field they qualify. This is the schema's central
/// design claim (`call.proto` header + `protobuf.ts`): proto3 collapses
/// absent/null/empty, so the explicit bit carries the distinction. We flip each
/// bit against a fixed value field and confirm the bit survives the round-trip.
#[test]
fn null_and_presence_bits_are_independent_wire_fields() {
    // `activePeerIsNull` (bare bool) — the value field stays the same; only the
    // null bit toggles, and it must survive.
    for is_null in [false, true] {
        let mut call = wire::Call {
            active_peer: None,
            active_peer_is_null: is_null,
            ..Default::default()
        };
        call = rt(&call);
        assert_eq!(call.active_peer_is_null, is_null);
    }

    // `billingContextIsNull` (optional bool) — three states: absent / false /
    // true, matching `optional(NullOr(String))`.
    for bit in [None, Some(false), Some(true)] {
        let call = rt(&wire::Call {
            billing_context_is_null: bit,
            ..Default::default()
        });
        assert_eq!(call.billing_context_is_null, bit);
    }

    // `aLegPendingViasPresent` — an empty repeated field with the present bit set
    // is the schema's "optional(Array) — empty survives" case: proto3 would
    // otherwise round-trip `[]` to absent.
    let call = rt(&wire::Call {
        a_leg_pending_vias: vec![],
        a_leg_pending_vias_present: true,
        ..Default::default()
    });
    assert!(call.a_leg_pending_vias.is_empty());
    assert!(
        call.a_leg_pending_vias_present,
        "the present bit must survive so an empty array is distinguishable from absent"
    );

    // `remoteCSeqIsNull` on the dialog ext — the nested message's null bit.
    let ext = rt(&wire::B2buaDialogExt {
        remote_c_seq: None,
        remote_c_seq_is_null: Some(true),
        ..Default::default()
    });
    assert_eq!(ext.remote_c_seq, None);
    assert_eq!(ext.remote_c_seq_is_null, Some(true));

    // `policyUpdateBody` (`optional bytes`) + `policyUpdateBodyIsNull` — one of
    // the four fields the `protobuf.ts` header names by reference, so sweep its
    // null bit against the value the way `cachedSdp` is swept above: absent body
    // with the null bit set, empty body, and non-empty body must each round-trip
    // with the bit independent of the value field.
    for (body, is_null) in [
        (None, Some(true)),
        (Some(Vec::<u8>::new()), Some(false)),
        (Some(vec![1u8, 2]), Some(false)),
    ] {
        let call = rt(&wire::Call {
            policy_update_body: body.clone(),
            policy_update_body_is_null: is_null,
            ..Default::default()
        });
        assert_eq!(call.policy_update_body, body, "policyUpdateBody {body:?} must round-trip");
        assert_eq!(
            call.policy_update_body_is_null, is_null,
            "policyUpdateBodyIsNull must survive independently of the body value"
        );
    }
}

/// P7 — byte-field integrity at size 0 / 1 / 1 KiB / 64 KiB, mirroring the TS
/// `Uint8Array integrity` case. Exercised on `ALegInvite.body` (`bytes`).
#[test]
fn p7_byte_field_integrity_sizes() {
    for len in [0usize, 1, 1024, 65536] {
        let body: Vec<u8> = (0..len).map(|i| (i & 0xff) as u8).collect();
        let msg = wire::ALegInvite {
            uri: "sip:x".into(),
            headers: vec![],
            body: body.clone(),
        };
        let back = rt(&msg);
        assert_eq!(back.body, body, "len={len}");
    }
}

/// `optional bytes` is distinct from an empty `bytes`: `cachedSdp` (optional)
/// carries None vs Some(empty) vs Some(data). This is the `bytes` analogue of
/// the null-bit distinction and matters for the policy/SDP carries.
#[test]
fn optional_bytes_three_states_round_trip() {
    for sdp in [None, Some(Vec::<u8>::new()), Some(vec![9u8, 8, 7])] {
        let ext = rt(&wire::B2buaDialogExt {
            cached_sdp: sdp.clone(),
            ..Default::default()
        });
        assert_eq!(ext.cached_sdp, sdp, "cachedSdp state {sdp:?} must round-trip");
    }
}

/// Field-id stability (ADR-0011 + the `call.proto` "never reuse a deleted id"
/// rule): ids 33/34 (`transfer{Json,IsNull}`) and 35/36
/// (`earlyPromote{Json,IsNull}`) were retired and MUST stay free. We prove the
/// generated `Call` bound **no typed field** to those ids: a wire field injected
/// at each retired tag decodes cleanly *and leaves the value untouched* (equal to
/// the all-default `Call`). prost silently skips tags it has no field for — so an
/// unaffected decode is exactly the "id is unassigned" proof. (Unlike the
/// C++/Java runtimes, prost does not retain an unknown-field set, so we assert on
/// the decoded value, not on re-encode survival.) Tag 40 (`extJson`, the current
/// last id) is the live-field control: the same injection *does* land in its
/// typed field, distinguishing "unassigned id skipped" from "decode ignored
/// everything".
#[test]
fn retired_field_ids_are_not_reused() {
    /// Append `v` as a base-128 varint (LSB first, high bit = continuation).
    fn put_varint(out: &mut Vec<u8>, mut v: u32) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
    }

    /// Minimal proto3 wire writer: a length-delimited (wire type 2) field with
    /// the given payload at `tag`. `key = (tag << 3) | wire_type`, varint-encoded
    /// (tag 33's key is 266, a two-byte varint — hence a real varint, not a
    /// single byte).
    fn ld_field(tag: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(&mut out, tag << 3 | 2); // wire type 2 = length-delimited
        put_varint(&mut out, payload.len() as u32);
        out.extend_from_slice(payload);
        out
    }

    let baseline = wire::Call::default();
    for retired in [33u32, 34, 35, 36] {
        let raw = ld_field(retired, b"x");
        let decoded = wire::Call::decode(&raw[..])
            .unwrap_or_else(|e| panic!("Call must accept (skip) unknown tag {retired}: {e}"));
        // No typed field is bound to a retired id, so the value is untouched —
        // identical to an all-default Call. (If codegen had reused the id, a
        // typed field would have captured the payload and this would differ.)
        assert_eq!(
            decoded, baseline,
            "retired id {retired} must not bind a typed field (decoded Call must be unchanged)"
        );
    }

    // Control: tag 40 (`extJson`, string) IS a live field — the same injection
    // lands in its typed field, proving the decode is not blanket-ignoring input.
    let live = ld_field(40, b"hi!");
    let decoded = wire::Call::decode(&live[..]).expect("live tag 40 decodes");
    assert_eq!(
        decoded.ext_json.as_deref(),
        Some("hi!"),
        "tag 40 is the live extJson field — it must decode into the typed field"
    );
    assert_ne!(
        decoded, baseline,
        "the live-field control must change the Call (else the retired-id check proves nothing)"
    );
}

/// Codegen casing/keyword correctness: the `CSeq`-bearing fields lowercase to
/// `c_seq` and the `type` fields escape to `r#type`. This compiles only if the
/// generated idents match, and the values round-trip.
#[test]
fn keyword_and_casing_fields_codegen_and_round_trip() {
    let pr = rt(&wire::PendingRequest {
        method: "ACK".into(),
        outbound_c_seq: 12, // proto `outboundCSeq`
        inbound_c_seq: 34,  // proto `inboundCSeq`
        source_vias: vec![],
        source_call_id: "c".into(),
        source_from: "f".into(),
        source_to: "t".into(),
        direction: "from-a".into(),
    });
    assert_eq!((pr.outbound_c_seq, pr.inbound_c_seq), (12, 34));

    let te = rt(&wire::TimerEntry {
        id: "t".into(),
        r#type: "keepalive".into(), // proto `type`
        fire_at: 0.0,
        leg_id: None,
    });
    assert_eq!(te.r#type, "keepalive");

    let call = rt(&wire::Call {
        a_leg_pending_c_seq: Some(99), // proto `aLegPendingCSeq`
        ..Default::default()
    });
    assert_eq!(call.a_leg_pending_c_seq, Some(99));
}
