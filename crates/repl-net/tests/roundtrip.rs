//! Round-trip, error-path, framing, and watermark-ordering tests for the
//! replication wire layer. The whole point of slice S2: prove encode→decode is
//! identity for every frame variant and that every malformed input is a typed
//! error, not a panic.

use std::sync::Arc;

use repl_net::codec::ReplCodecError;
use repl_net::framing::ReplFramingError;
use repl_net::{
    decode_frame, encode_frame, frame_with_len_prefix, try_read_framed, Frame, Op, Partition,
    PullMode, Watermark, MAX_FRAME_LEN,
};

fn assert_roundtrip(f: &Frame) {
    let bytes = encode_frame(f);
    let back = decode_frame(&bytes).expect("decode should succeed");
    assert_eq!(*f, back, "round-trip mismatch for {f:?}");
    // Byte-determinism: encoding twice yields identical bytes.
    let bytes2 = encode_frame(f);
    assert_eq!(bytes, bytes2, "encode is not deterministic for {f:?}");
}

// --- round-trip: every variant + edge cases --------------------------------

#[test]
fn roundtrip_pull_request_replog() {
    assert_roundtrip(&Frame::PullRequest {
        proto_ver: 1,
        caller: "worker-3".into(),
        mode: PullMode::Replog,
        since: Watermark::new(7, 42),
        chunk: 128,
    });
}

#[test]
fn roundtrip_pull_request_bootstrap_empty_caller() {
    assert_roundtrip(&Frame::PullRequest {
        proto_ver: u16::MAX,
        caller: String::new(),
        mode: PullMode::Bootstrap,
        since: Watermark::new(0, 0),
        chunk: u32::MAX,
    });
}

#[test]
fn roundtrip_ack_large_watermark() {
    assert_roundtrip(&Frame::Ack {
        caller: "w".into(),
        up_to: Watermark::new(u64::MAX, u64::MAX),
    });
}

#[test]
fn roundtrip_data_body_some_multi_index() {
    assert_roundtrip(&Frame::Data {
        at: Watermark::new(3, 9),
        op: Op::Create,
        partition: Partition::Bak,
        call_ref: "pri1|callid|fromtag".into(),
        call_gen: 17,
        body_ttl_ms: 30_000,
        indexes: vec!["idx:a".into(), "idx:b".into(), "idx:c".into()],
        body: Some(Arc::from(&b"\x00\x01\x02encoded-call\xff"[..])),
    });
}

#[test]
fn roundtrip_data_body_none_delete_negatives() {
    // delete → body None; negative call_gen / body_ttl_ms (i64 on the wire).
    assert_roundtrip(&Frame::Data {
        at: Watermark::new(0, 0),
        op: Op::Delete,
        partition: Partition::Pri,
        call_ref: String::new(),
        call_gen: -1,
        body_ttl_ms: i64::MIN,
        indexes: vec![],
        body: None,
    });
}

#[test]
fn roundtrip_data_empty_body_is_some_not_none() {
    // An empty bin must round-trip as Some(empty), distinct from None.
    let f = Frame::Data {
        at: Watermark::new(1, 1),
        op: Op::Update,
        partition: Partition::Pri,
        call_ref: "x".into(),
        call_gen: 0,
        body_ttl_ms: 0,
        indexes: vec!["only".into()],
        body: Some(Arc::from(&[][..])),
    };
    assert_roundtrip(&f);
    if let Frame::Data { body, .. } = decode_frame(&encode_frame(&f)).unwrap() {
        assert_eq!(body, Some(Arc::from(&[][..])));
    } else {
        panic!("expected Data");
    }
}

#[test]
fn roundtrip_data_large_body() {
    let big: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
    assert_roundtrip(&Frame::Data {
        at: Watermark::new(u64::MAX, 0),
        op: Op::Update,
        partition: Partition::Bak,
        call_ref: "big".into(),
        call_gen: i64::MAX,
        body_ttl_ms: i64::MAX,
        indexes: vec![String::new()], // empty-string index
        body: Some(Arc::from(big.as_slice())),
    });
}

#[test]
fn roundtrip_noop() {
    assert_roundtrip(&Frame::Noop {
        at: Watermark::new(5, 0),
    });
}

#[test]
fn roundtrip_reset_to_bootstrap() {
    assert_roundtrip(&Frame::ResetToBootstrap {
        reason: "since fell off compacted tail".into(),
    });
    assert_roundtrip(&Frame::ResetToBootstrap {
        reason: String::new(),
    });
}

// --- error paths: no panic, typed error ------------------------------------

#[test]
fn err_unknown_tag() {
    // [9, ...] — tag 9 is not one of the five.
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 2).unwrap();
    rmp::encode::write_uint(&mut bytes, 9).unwrap();
    rmp::encode::write_uint(&mut bytes, 0).unwrap();
    match decode_frame(&bytes) {
        Err(ReplCodecError::UnknownTag(9)) => {}
        other => panic!("expected UnknownTag(9), got {other:?}"),
    }
}

#[test]
fn err_unknown_enum_discriminant_mode() {
    // PullRequest with mode = 7.
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 7).unwrap();
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // tag PullRequest
    rmp::encode::write_uint(&mut bytes, 1).unwrap(); // proto_ver
    rmp::encode::write_str(&mut bytes, "w").unwrap(); // caller
    rmp::encode::write_uint(&mut bytes, 7).unwrap(); // mode = 7 (invalid)
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // since_gen
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // since_counter
    rmp::encode::write_uint(&mut bytes, 1).unwrap(); // chunk
    match decode_frame(&bytes) {
        Err(ReplCodecError::UnknownDiscriminant {
            field: "PullMode",
            value: 7,
        }) => {}
        other => panic!("expected UnknownDiscriminant PullMode 7, got {other:?}"),
    }
}

#[test]
fn err_unknown_enum_discriminant_op() {
    // Data with op = 5.
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 10).unwrap();
    rmp::encode::write_uint(&mut bytes, 2).unwrap(); // tag Data
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // gen
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // counter
    rmp::encode::write_uint(&mut bytes, 5).unwrap(); // op = 5 (invalid)
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // partition
    rmp::encode::write_str(&mut bytes, "r").unwrap(); // call_ref
    rmp::encode::write_sint(&mut bytes, 0).unwrap(); // call_gen
    rmp::encode::write_sint(&mut bytes, 0).unwrap(); // body_ttl_ms
    rmp::encode::write_array_len(&mut bytes, 0).unwrap(); // indexes
    rmp::encode::write_nil(&mut bytes).unwrap(); // body
    match decode_frame(&bytes) {
        Err(ReplCodecError::UnknownDiscriminant {
            field: "Op",
            value: 5,
        }) => {}
        other => panic!("expected UnknownDiscriminant Op 5, got {other:?}"),
    }
}

#[test]
fn err_wrong_array_length() {
    // Noop must be len 3; give it 2.
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 2).unwrap();
    rmp::encode::write_uint(&mut bytes, 3).unwrap(); // tag Noop
    rmp::encode::write_uint(&mut bytes, 0).unwrap(); // gen only
    match decode_frame(&bytes) {
        Err(ReplCodecError::MalformedArray(_)) => {}
        other => panic!("expected MalformedArray, got {other:?}"),
    }
}

#[test]
fn err_truncated_payload() {
    // Valid Ack header but cut off mid-frame.
    let full = encode_frame(&Frame::Ack {
        caller: "worker".into(),
        up_to: Watermark::new(9, 9),
    });
    let truncated = &full[..full.len() - 1];
    match decode_frame(truncated) {
        Err(ReplCodecError::Truncated(_)) | Err(ReplCodecError::Type { .. }) => {}
        other => panic!("expected Truncated/Type, got {other:?}"),
    }
}

#[test]
fn err_not_an_array() {
    // A bare integer is not a frame array.
    let mut bytes = Vec::new();
    rmp::encode::write_uint(&mut bytes, 42).unwrap();
    match decode_frame(&bytes) {
        Err(ReplCodecError::Type { .. }) | Err(ReplCodecError::Truncated(_)) => {}
        other => panic!("expected Type/Truncated, got {other:?}"),
    }
}

#[test]
fn err_empty_input() {
    match decode_frame(&[]) {
        Err(ReplCodecError::Truncated(_)) => {}
        other => panic!("expected Truncated, got {other:?}"),
    }
}

// --- length-prefix framing -------------------------------------------------

#[test]
fn framing_wrap_then_read_one() {
    let payload = encode_frame(&Frame::Noop {
        at: Watermark::new(1, 2),
    });
    let wire = frame_with_len_prefix(&payload);
    assert_eq!(&wire[..4], &(payload.len() as u32).to_be_bytes());

    let mut buf = wire.clone();
    let got = try_read_framed(&mut buf).unwrap().expect("one frame");
    assert_eq!(got, payload);
    assert!(buf.is_empty(), "buffer fully drained");
    // Empty buffer → None.
    assert!(try_read_framed(&mut buf).unwrap().is_none());
}

#[test]
fn framing_two_concatenated_pop_in_order() {
    let a = encode_frame(&Frame::Noop {
        at: Watermark::new(1, 0),
    });
    let b = encode_frame(&Frame::ResetToBootstrap {
        reason: "x".into(),
    });
    let mut buf = Vec::new();
    buf.extend_from_slice(&frame_with_len_prefix(&a));
    buf.extend_from_slice(&frame_with_len_prefix(&b));

    let first = try_read_framed(&mut buf).unwrap().unwrap();
    let second = try_read_framed(&mut buf).unwrap().unwrap();
    assert_eq!(first, a);
    assert_eq!(second, b);
    assert!(try_read_framed(&mut buf).unwrap().is_none());
    // Decodes back to the original frames.
    assert_eq!(
        decode_frame(&first).unwrap(),
        Frame::Noop {
            at: Watermark::new(1, 0)
        }
    );
}

#[test]
fn framing_partial_then_completes() {
    let payload = encode_frame(&Frame::Ack {
        caller: "abc".into(),
        up_to: Watermark::new(2, 2),
    });
    let wire = frame_with_len_prefix(&payload);

    // Only the first 2 bytes of the length prefix have arrived.
    let mut buf = wire[..2].to_vec();
    assert!(try_read_framed(&mut buf).unwrap().is_none());

    // Length prefix complete, payload partially arrived.
    buf.extend_from_slice(&wire[2..6]);
    assert!(try_read_framed(&mut buf).unwrap().is_none());

    // Rest arrives → frame pops.
    buf.extend_from_slice(&wire[6..]);
    let got = try_read_framed(&mut buf).unwrap().unwrap();
    assert_eq!(got, payload);
}

#[test]
fn framing_oversized_prefix_errors() {
    // A length prefix above the cap, no payload bytes needed.
    let mut buf = (MAX_FRAME_LEN + 1).to_be_bytes().to_vec();
    match try_read_framed(&mut buf) {
        Err(ReplFramingError::Oversized { len }) => assert_eq!(len, MAX_FRAME_LEN + 1),
        other => panic!("expected Oversized, got {other:?}"),
    }
    // Buffer left intact for caller inspection.
    assert_eq!(buf.len(), 4);
}

#[test]
fn framing_empty_payload_roundtrips() {
    let wire = frame_with_len_prefix(&[]);
    assert_eq!(wire, vec![0, 0, 0, 0]);
    let mut buf = wire;
    let got = try_read_framed(&mut buf).unwrap().unwrap();
    assert!(got.is_empty());
}

// --- watermark ordering: the reboot-incarnation rule -----------------------

#[test]
fn watermark_gen_is_high_word() {
    // (1, 0) > (0, u64::MAX): a new incarnation beats any prior counter.
    assert!(Watermark::new(1, 0) > Watermark::new(0, u64::MAX));
    // Within an incarnation, counter orders.
    assert!(Watermark::new(5, 10) > Watermark::new(5, 9));
    assert!(Watermark::new(5, 9) < Watermark::new(5, 10));
    // Equality.
    assert_eq!(Watermark::new(7, 7), Watermark::new(7, 7));
    // Lexicographic, explicit.
    assert!(Watermark::new(2, 0) > Watermark::new(1, u64::MAX));
}
