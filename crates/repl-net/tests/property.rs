//! Property test: encode→decode is identity for arbitrary frames, and encoding
//! is byte-deterministic. Bodies are arbitrary bytes; strings/ints arbitrary.

use std::sync::Arc;

use proptest::prelude::*;

use repl_net::{decode_frame, encode_frame, Frame, Op, Partition, Watermark};

fn watermark_strat() -> impl Strategy<Value = Watermark> {
    (any::<u64>(), any::<u64>()).prop_map(|(g, c)| Watermark::new(g, c))
}

fn op_strat() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Put), Just(Op::Delete)]
}

fn partition_strat() -> impl Strategy<Value = Partition> {
    prop_oneof![Just(Partition::Pri), Just(Partition::Bak)]
}

fn frame_strat() -> impl Strategy<Value = Frame> {
    let pull = (any::<u16>(), ".*", partition_strat(), watermark_strat())
        .prop_map(|(proto_ver, caller, partition, since)| Frame::PullRequest {
            proto_ver,
            caller,
            partition,
            since,
        });

    let data = (
        watermark_strat(),
        op_strat(),
        partition_strat(),
        ".*",
        any::<i64>(),
        any::<i64>(),
        any::<i64>(),
        proptest::collection::vec(".*", 0..5),
        proptest::option::of(proptest::collection::vec(any::<u8>(), 0..256)),
    )
        .prop_map(
            |(at, op, partition, call_ref, call_gen, call_bgen, body_ttl_ms, indexes, body)| {
                Frame::Data {
                    at,
                    op,
                    partition,
                    call_ref,
                    call_gen,
                    call_bgen,
                    body_ttl_ms,
                    indexes,
                    body: body.map(|b| Arc::from(b.as_slice())),
                }
            },
        );

    let noop = watermark_strat().prop_map(|at| Frame::Noop { at });

    let reset = ".*".prop_map(|reason| Frame::ResetToBootstrap { reason });

    prop_oneof![pull, data, noop, reset]
}

proptest! {
    #[test]
    fn roundtrip_identity(f in frame_strat()) {
        let bytes = encode_frame(&f);
        let back = decode_frame(&bytes).expect("decode must succeed for an encoded frame");
        prop_assert_eq!(f, back);
    }

    #[test]
    fn encode_is_deterministic(f in frame_strat()) {
        prop_assert_eq!(encode_frame(&f), encode_frame(&f));
    }
}
