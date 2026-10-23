//! 4-byte big-endian length-prefix framing.
//!
//! The real TCP transport carries a *stream* of bytes; this module
//! delimits it into discrete frame payloads. Each message on the wire is
//! `[u32 BE length][payload]`. [`frame_with_len_prefix`] wraps one payload;
//! [`try_read_framed`] pops exactly one complete payload off a growing receive
//! buffer (or returns `None` if it has not all arrived yet) — the shape the
//! TCP reader loop calls.
//!
//! The sim transport moves whole `Vec<u8>` frames and does **not**
//! need this — length-prefixing is the real-transport-only concern, unit-tested
//! here in isolation (ADR-0011 X2 / the migration plan).

/// Hard cap on a single framed payload (64 MiB). A larger length prefix is
/// rejected rather than trusted — it is almost certainly a desync or a hostile
/// peer, and we will not pre-allocate against it.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Failure framing a payload or reading a length-prefixed frame off a stream
/// buffer.
#[derive(Debug, thiserror::Error)]
pub enum ReplFramingError {
    /// The payload's length, to send or advertised, exceeds [`MAX_FRAME_LEN`].
    #[error("framed length {len} exceeds max {MAX_FRAME_LEN}")]
    Oversized {
        /// The payload length.
        len: usize,
    },
}

/// Wrap a payload as `[u32 BE length][payload]`. A payload past
/// [`MAX_FRAME_LEN`] is refused: the receiving end would cut the stream on it.
pub fn frame_with_len_prefix(payload: &[u8]) -> Result<Vec<u8>, ReplFramingError> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
        .ok_or(ReplFramingError::Oversized { len: payload.len() })?;
    let mut out = Vec::with_capacity(payload.len().saturating_add(4));
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
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
/// This is the streaming-decoder primitive the real TCP reader loops on:
/// append socket reads to `buf`, then drain frames until it returns `None`.
pub fn try_read_framed(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ReplFramingError> {
    let Some(prefix) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(*prefix) as usize;
    if len > MAX_FRAME_LEN as usize {
        return Err(ReplFramingError::Oversized { len });
    }
    let total = len.saturating_add(4);
    let Some(payload) = buf.get(4..total).map(<[u8]>::to_vec) else {
        // Header present but payload incomplete.
        return Ok(None);
    };
    // Drain the consumed frame from the front, keeping any trailing bytes
    // (the start of the next frame) for the following call.
    #[expect(clippy::disallowed_methods, reason = "the range ends inside the buffer, read above")]
    buf.drain(..total);
    Ok(Some(payload))
}
