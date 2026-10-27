//! One case of the frame-codec lane and its fuzz target: the stream reader
//! the real transport runs over a peer's bytes, and the decoder over the
//! bytes after the first prefix.

use repl_net::{decode_frame, try_read_framed, Frame};

/// The frames the stream reader decodes off `wire`, in order, and the frame
/// the bytes after the first prefix decode to, if any.
pub fn case(wire: &[u8]) -> (Vec<Frame>, Option<Frame>) {
    let mut buf = wire.to_vec();
    let mut frames = Vec::new();
    while let Ok(Some(payload)) = try_read_framed(&mut buf) {
        match decode_frame(&payload) {
            Ok(frame) => frames.push(frame),
            Err(_) => break,
        }
    }
    let bare = decode_frame(wire.get(4..).unwrap_or_default()).ok();
    (frames, bare)
}
