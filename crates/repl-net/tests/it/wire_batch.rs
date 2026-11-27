//! A batch's frames encoded into one buffer are the bytes each frame frames
//! to alone, back to back, and a frame that cannot go leaves the buffer as it
//! was.

use std::sync::Arc;

use repl_net::transport::{append_wire_bytes, SendError};
use repl_net::{
    encode_frame, frame_with_len_prefix, Frame, Op, Partition, Watermark, MAX_FRAME_LEN,
};

fn put(call_ref: &str, body: Vec<u8>) -> Frame {
    Frame::Data {
        at: Watermark::new(1, 7),
        op: Op::Put,
        partition: Partition::Bak,
        call_ref: call_ref.into(),
        call_gen: 3,
        call_bgen: 1,
        body_ttl_ms: 60_000,
        origin_now_ms: 1_000,
        indexes: vec!["sip:a".into()],
        body: Some(Arc::from(body)),
        answered: false,
        incarnation: Some("inc".into()),
    }
}

fn alone(frame: &Frame) -> Vec<u8> {
    frame_with_len_prefix(&encode_frame(frame).unwrap()).unwrap()
}

#[test]
fn a_batch_buffer_holds_each_frame_as_it_frames_alone() {
    let frames = [
        put("a", vec![1; 10]),
        Frame::Noop { at: Watermark::new(1, 8) },
        put("b", vec![2; 70_000]),
    ];
    let mut buf = Vec::new();
    for frame in &frames {
        append_wire_bytes(frame, &mut buf).unwrap();
    }
    let expected: Vec<u8> = frames.iter().flat_map(alone).collect();
    assert_eq!(buf, expected);
}

#[test]
fn a_frame_past_the_cap_leaves_the_batch_buffer_as_it_was() {
    let mut buf = Vec::new();
    append_wire_bytes(&put("a", vec![1; 10]), &mut buf).unwrap();
    let before = buf.clone();
    let refused = append_wire_bytes(&put("huge", vec![0; MAX_FRAME_LEN as usize]), &mut buf);
    assert!(matches!(refused, Err(SendError::Unencodable(_))), "{refused:?}");
    assert_eq!(buf, before);
}
