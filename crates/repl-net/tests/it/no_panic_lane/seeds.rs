//! The synthetic seeds the frame-codec lane mutates: one stream per frame
//! shape a peer sends, each frame encoded and length-prefixed as the real
//! transport writes it, and one stream of two frames back to back.

use std::sync::Arc;

use repl_net::{encode_frame, frame_with_len_prefix, Frame, Op, Partition, Watermark};

/// One seed: a stable name, the frames it carries, and their wire bytes.
pub struct Seed {
    pub name: &'static str,
    pub frames: Vec<Frame>,
    pub wire: Vec<u8>,
}

fn seed(name: &'static str, frames: Vec<Frame>) -> Seed {
    let mut wire = Vec::new();
    for frame in &frames {
        let payload = encode_frame(frame).expect("a seed frame encodes");
        wire.extend(frame_with_len_prefix(&payload).expect("a seed frame fits"));
    }
    Seed { name, frames, wire }
}

fn data(op: Op, body: Option<&[u8]>, incarnation: Option<&str>) -> Frame {
    Frame::Data {
        at: Watermark::new(3, 17),
        op,
        partition: Partition::Bak,
        call_ref: "primary|lane-call-1@192.0.2.20|a1".into(),
        call_gen: 4,
        call_bgen: 2,
        body_ttl_ms: 30_000,
        origin_now_ms: 1_700_000_000_000,
        indexes: vec!["dialog|lane-call-1@192.0.2.20|a1|b2".into(), "ref|x".into()],
        body: body.map(Arc::from),
        answered: true,
        incarnation: incarnation.map(String::from),
    }
}

/// Every seed.
pub fn all() -> Vec<Seed> {
    vec![
        seed(
            "pull-request",
            vec![Frame::PullRequest {
                proto_ver: 3,
                caller: "w-1".into(),
                partition: Partition::Pri,
                since: Watermark::new(2, 9),
            }],
        ),
        seed("data/put", vec![data(Op::Put, Some(b"\x93\x01\xa1x\xc0"), Some("w-0#7"))]),
        seed("data/delete", vec![data(Op::Delete, None, None)]),
        seed("noop", vec![Frame::Noop { at: Watermark::new(3, 18) }]),
        seed("position", vec![Frame::Position { at: Watermark::new(3, 19) }]),
        seed(
            "reset-to-bootstrap",
            vec![Frame::ResetToBootstrap { reason: "since fell off the tail".into() }],
        ),
        seed(
            "stream/noop-then-put",
            vec![Frame::Noop { at: Watermark::new(1, 1) }, data(Op::Put, Some(b""), None)],
        ),
    ]
}
