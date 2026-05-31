//! 4-byte big-endian length-prefix framing.
//!
//! The real TCP transport (slice S3) carries a *stream* of bytes; this module
//! delimits it into discrete frame payloads. Each message on the wire is
//! `[u32 BE length][payload]`. [`frame_with_len_prefix`] wraps one payload;
//! [`try_read_framed`] pops exactly one complete payload off a growing receive
//! buffer (or returns `None` if it has not all arrived yet) — the shape S3's
//! TCP reader loop will call.
//!
//! The sim transport (also S3) moves whole `Vec<u8>` frames and does **not**
//! need this — length-prefixing is the real-transport-only concern, unit-tested
//! here in isolation (ADR-0011 X2 / the migration plan).

/// Hard cap on a single framed payload (64 MiB). A larger length prefix is
/// rejected rather than trusted — it is almost certainly a desync or a hostile
/// peer, and we will not pre-allocate against it.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Failure reading a length-prefixed frame off a stream buffer.
#[derive(Debug, thiserror::Error)]
pub enum ReplFramingError {
    /// The advertised length exceeds [`MAX_FRAME_LEN`].
    #[error("framed length {len} exceeds max {MAX_FRAME_LEN}")]
    Oversized {
        /// The advertised payload length.
        len: u32,
    },
}

/// Wrap a payload as `[u32 BE length][payload]`.
///
/// # Panics
/// Never on valid input; a payload longer than `u32::MAX` cannot occur in this
/// codebase (frames are far smaller), but is debug-asserted for safety.
pub fn frame_with_len_prefix(payload: &[u8]) -> Vec<u8> {
    debug_assert!(
        payload.len() <= u32::MAX as usize,
        "payload longer than u32::MAX cannot be length-prefixed"
    );
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Try to pop one complete length-prefixed payload off the front of `buf`.
///
/// - `Ok(Some(payload))` — a full frame was present; its bytes (prefix +
///   payload) are drained from the front of `buf` and the payload returned.
/// - `Ok(None)` — the buffer does not yet hold a full frame (fewer than 4
///   length bytes, or the payload has not all arrived). `buf` is left intact;
///   call again once more bytes have been appended.
/// - `Err(Oversized)` — the length prefix exceeds [`MAX_FRAME_LEN`]; the caller
///   should drop the connection (the stream is desynced or hostile). `buf` is
///   left intact so the caller can inspect it.
///
/// This is the streaming-decoder primitive S3's real TCP reader loops on:
/// append socket reads to `buf`, then drain frames until it returns `None`.
pub fn try_read_framed(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ReplFramingError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_LEN {
        return Err(ReplFramingError::Oversized { len });
    }
    let total = 4 + len as usize;
    if buf.len() < total {
        // Header present but payload incomplete.
        return Ok(None);
    }
    let payload = buf[4..total].to_vec();
    // Drain the consumed frame from the front, keeping any trailing bytes
    // (the start of the next frame) for the following call.
    buf.drain(..total);
    Ok(Some(payload))
}
