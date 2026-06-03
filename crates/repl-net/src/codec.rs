//! Single-frame positional-msgpack codec.
//!
//! Encode/decode is **hand-rolled** over the low-level `rmp` crate so the wire
//! layout is exactly the tag-led array in ADR-0011 X9 (a `serde`-derived enum
//! would not emit clean `[tag, ...]` arrays). Encoding is **deterministic**:
//! the same [`Frame`] always produces identical bytes — `rmp`'s `write_*`
//! helpers pick the minimal marker for each value, and we never iterate an
//! unordered collection.

use std::sync::Arc;

use rmp::decode::{self, NumValueReadError, ValueReadError};
use rmp::encode;
use rmp::Marker;

use crate::frame::{tag, Frame, Op, Partition, PullMode, UnknownDiscriminant, Watermark};

/// Failure decoding a single replication frame. Every malformed input lands
/// here as a typed error — the decoder never panics.
#[derive(Debug, thiserror::Error)]
pub enum ReplCodecError {
    /// The outer value was not a msgpack array, or its length did not match the
    /// tag's fixed arity.
    #[error("malformed frame array: {0}")]
    MalformedArray(String),
    /// Element 0 was not one of the known frame tags.
    #[error("unknown frame tag: {0}")]
    UnknownTag(u64),
    /// An enum-coded byte (mode / op / partition) was out of range.
    #[error("unknown discriminant for {field}: {value}")]
    UnknownDiscriminant {
        /// Which field rejected the byte.
        field: &'static str,
        /// The offending value.
        value: u8,
    },
    /// A value had the wrong msgpack type or was out of range for its Rust type.
    #[error("type error at {at}: {detail}")]
    Type {
        /// Which element failed (for diagnostics).
        at: &'static str,
        /// What went wrong.
        detail: String,
    },
    /// The buffer ended before the frame was fully read.
    #[error("truncated frame: {0}")]
    Truncated(String),
    /// A string element was not valid UTF-8.
    #[error("invalid utf-8 in {at}")]
    Utf8 {
        /// Which element held the bad bytes.
        at: &'static str,
    },
}

impl From<UnknownDiscriminant> for ReplCodecError {
    fn from(u: UnknownDiscriminant) -> Self {
        ReplCodecError::UnknownDiscriminant {
            field: u.field,
            value: u.value,
        }
    }
}

// --- encode ----------------------------------------------------------------

/// Encode a single frame to a fresh `Vec<u8>` (one positional-msgpack array).
///
/// Infallible: writing to a `Vec` cannot fail, and every value is in range for
/// its marker. Deterministic: same frame → identical bytes.
pub fn encode_frame(frame: &Frame) -> Vec<u8> {
    let mut buf = Vec::new();
    write_frame(&mut buf, frame);
    buf
}

fn write_frame(buf: &mut Vec<u8>, frame: &Frame) {
    match frame {
        Frame::PullRequest {
            proto_ver,
            caller,
            mode,
            since,
            chunk,
        } => {
            encode::write_array_len(buf, 7).unwrap();
            encode::write_uint(buf, tag::PULL_REQUEST).unwrap();
            encode::write_uint(buf, *proto_ver as u64).unwrap();
            encode::write_str(buf, caller).unwrap();
            encode::write_uint(buf, mode.as_u8() as u64).unwrap();
            encode::write_uint(buf, since.gen).unwrap();
            encode::write_uint(buf, since.counter).unwrap();
            encode::write_uint(buf, *chunk as u64).unwrap();
        }
        Frame::Ack { caller, up_to } => {
            encode::write_array_len(buf, 4).unwrap();
            encode::write_uint(buf, tag::ACK).unwrap();
            encode::write_str(buf, caller).unwrap();
            encode::write_uint(buf, up_to.gen).unwrap();
            encode::write_uint(buf, up_to.counter).unwrap();
        }
        Frame::Data {
            at,
            op,
            partition,
            call_ref,
            call_gen,
            body_ttl_ms,
            indexes,
            body,
        } => {
            encode::write_array_len(buf, 10).unwrap();
            encode::write_uint(buf, tag::DATA).unwrap();
            encode::write_uint(buf, at.gen).unwrap();
            encode::write_uint(buf, at.counter).unwrap();
            encode::write_uint(buf, op.as_u8() as u64).unwrap();
            encode::write_uint(buf, partition.as_u8() as u64).unwrap();
            encode::write_str(buf, call_ref).unwrap();
            encode::write_sint(buf, *call_gen).unwrap();
            encode::write_sint(buf, *body_ttl_ms).unwrap();
            encode::write_array_len(buf, indexes.len() as u32).unwrap();
            for idx in indexes {
                encode::write_str(buf, idx).unwrap();
            }
            match body {
                Some(bytes) => {
                    encode::write_bin(buf, bytes).unwrap();
                }
                None => {
                    encode::write_nil(buf).unwrap();
                }
            }
        }
        Frame::Noop { at } => {
            encode::write_array_len(buf, 3).unwrap();
            encode::write_uint(buf, tag::NOOP).unwrap();
            encode::write_uint(buf, at.gen).unwrap();
            encode::write_uint(buf, at.counter).unwrap();
        }
        Frame::ResetToBootstrap { reason } => {
            encode::write_array_len(buf, 2).unwrap();
            encode::write_uint(buf, tag::RESET_TO_BOOTSTRAP).unwrap();
            encode::write_str(buf, reason).unwrap();
        }
        Frame::Deactivate { as_of_ms } => {
            encode::write_array_len(buf, 2).unwrap();
            encode::write_uint(buf, tag::DEACTIVATE).unwrap();
            encode::write_sint(buf, *as_of_ms).unwrap();
        }
    }
}

// --- decode ----------------------------------------------------------------

/// Decode a single frame from a complete byte slice.
///
/// `bytes` must hold exactly one frame (length-prefix framing in
/// [`crate::framing`] is what splits a stream into these slices). Trailing
/// bytes after a complete frame are ignored — the array-len/element contract is
/// self-delimiting.
pub fn decode_frame(bytes: &[u8]) -> Result<Frame, ReplCodecError> {
    let mut rd: &[u8] = bytes;
    let len = read_array_len(&mut rd, "frame")?;
    if len == 0 {
        return Err(ReplCodecError::MalformedArray(
            "empty array, no tag element".into(),
        ));
    }
    let tag = read_u64(&mut rd, "tag")?;
    match tag {
        tag::PULL_REQUEST => decode_pull_request(&mut rd, len),
        tag::ACK => decode_ack(&mut rd, len),
        tag::DATA => decode_data(&mut rd, len),
        tag::NOOP => decode_noop(&mut rd, len),
        tag::RESET_TO_BOOTSTRAP => decode_reset(&mut rd, len),
        tag::DEACTIVATE => decode_deactivate(&mut rd, len),
        other => Err(ReplCodecError::UnknownTag(other)),
    }
}

fn expect_len(got: u32, want: u32, frame: &'static str) -> Result<(), ReplCodecError> {
    if got != want {
        return Err(ReplCodecError::MalformedArray(format!(
            "{frame} expects array len {want}, got {got}"
        )));
    }
    Ok(())
}

fn decode_pull_request(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 7, "PullRequest")?;
    let proto_ver = read_u16(rd, "proto_ver")?;
    let caller = read_str(rd, "caller")?;
    let mode = PullMode::from_u8(read_u8(rd, "mode")?)?;
    let since_gen = read_u64(rd, "since_gen")?;
    let since_counter = read_u64(rd, "since_counter")?;
    let chunk = read_u32(rd, "chunk")?;
    Ok(Frame::PullRequest {
        proto_ver,
        caller,
        mode,
        since: Watermark::new(since_gen, since_counter),
        chunk,
    })
}

fn decode_ack(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 4, "Ack")?;
    let caller = read_str(rd, "caller")?;
    let gen = read_u64(rd, "up_to_gen")?;
    let counter = read_u64(rd, "up_to_counter")?;
    Ok(Frame::Ack {
        caller,
        up_to: Watermark::new(gen, counter),
    })
}

fn decode_data(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 10, "Data")?;
    let gen = read_u64(rd, "gen")?;
    let counter = read_u64(rd, "counter")?;
    let op = Op::from_u8(read_u8(rd, "op")?)?;
    let partition = Partition::from_u8(read_u8(rd, "partition")?)?;
    let call_ref = read_str(rd, "call_ref")?;
    let call_gen = read_i64(rd, "call_gen")?;
    let body_ttl_ms = read_i64(rd, "body_ttl_ms")?;
    let idx_len = read_array_len(rd, "indexes")?;
    // Clamp the pre-allocation to the bytes still in the buffer: every msgpack
    // element costs >= 1 byte, so a legitimate count can never exceed `rd.len()`.
    // A hostile/desynced Array32 count (up to u32::MAX) therefore cannot force a
    // multi-GB allocation; a genuinely truncated frame still errors in the loop.
    let mut indexes = Vec::with_capacity((idx_len as usize).min(rd.len()));
    for _ in 0..idx_len {
        indexes.push(read_str(rd, "indexes[]")?);
    }
    let body = read_opt_bin(rd, "body")?;
    Ok(Frame::Data {
        at: Watermark::new(gen, counter),
        op,
        partition,
        call_ref,
        call_gen,
        body_ttl_ms,
        indexes,
        body,
    })
}

fn decode_noop(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 3, "Noop")?;
    let gen = read_u64(rd, "gen")?;
    let counter = read_u64(rd, "counter")?;
    Ok(Frame::Noop {
        at: Watermark::new(gen, counter),
    })
}

fn decode_reset(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 2, "ResetToBootstrap")?;
    let reason = read_str(rd, "reason")?;
    Ok(Frame::ResetToBootstrap { reason })
}

fn decode_deactivate(rd: &mut &[u8], len: u32) -> Result<Frame, ReplCodecError> {
    expect_len(len, 2, "Deactivate")?;
    let as_of_ms = read_i64(rd, "as_of_ms")?;
    Ok(Frame::Deactivate { as_of_ms })
}

// --- low-level readers over a `&mut &[u8]` cursor --------------------------
//
// `&[u8]` implements `rmp::RmpRead`, advancing the cursor as it consumes. We
// map its two error families to our typed `Truncated` / `Type` errors so no
// path can panic.

fn map_vre(e: ValueReadError, at: &'static str) -> ReplCodecError {
    match e {
        ValueReadError::InvalidMarkerRead(_) | ValueReadError::InvalidDataRead(_) => {
            ReplCodecError::Truncated(format!("at {at}"))
        }
        ValueReadError::TypeMismatch(m) => ReplCodecError::Type {
            at,
            detail: format!("unexpected marker {m:?}"),
        },
    }
}

fn map_nvre(e: NumValueReadError, at: &'static str) -> ReplCodecError {
    match e {
        NumValueReadError::InvalidMarkerRead(_) | NumValueReadError::InvalidDataRead(_) => {
            ReplCodecError::Truncated(format!("at {at}"))
        }
        NumValueReadError::TypeMismatch(m) => ReplCodecError::Type {
            at,
            detail: format!("unexpected marker {m:?}"),
        },
        NumValueReadError::OutOfRange => ReplCodecError::Type {
            at,
            detail: "integer out of range".into(),
        },
    }
}

fn read_array_len(rd: &mut &[u8], at: &'static str) -> Result<u32, ReplCodecError> {
    decode::read_array_len(rd).map_err(|e| map_vre(e, at))
}

fn read_u64(rd: &mut &[u8], at: &'static str) -> Result<u64, ReplCodecError> {
    decode::read_int(rd).map_err(|e| map_nvre(e, at))
}

fn read_u32(rd: &mut &[u8], at: &'static str) -> Result<u32, ReplCodecError> {
    decode::read_int(rd).map_err(|e| map_nvre(e, at))
}

fn read_u16(rd: &mut &[u8], at: &'static str) -> Result<u16, ReplCodecError> {
    decode::read_int(rd).map_err(|e| map_nvre(e, at))
}

fn read_u8(rd: &mut &[u8], at: &'static str) -> Result<u8, ReplCodecError> {
    decode::read_int(rd).map_err(|e| map_nvre(e, at))
}

fn read_i64(rd: &mut &[u8], at: &'static str) -> Result<i64, ReplCodecError> {
    decode::read_int(rd).map_err(|e| map_nvre(e, at))
}

/// Read a msgpack `str`: read its length marker, then split that many bytes off
/// the cursor and validate UTF-8.
fn read_str(rd: &mut &[u8], at: &'static str) -> Result<String, ReplCodecError> {
    let n = decode::read_str_len(rd).map_err(|e| map_vre(e, at))? as usize;
    let raw = take(rd, n, at)?;
    String::from_utf8(raw.to_vec()).map_err(|_| ReplCodecError::Utf8 { at })
}

/// Read a msgpack `bin` (→ `Some`) or `nil` (→ `None`). Peeks the marker so a
/// `nil` is distinguished from an empty `bin` (`Some(empty)`).
fn read_opt_bin(rd: &mut &[u8], at: &'static str) -> Result<Option<Arc<[u8]>>, ReplCodecError> {
    // Peek the marker without consuming more than it.
    let marker = decode::read_marker(rd).map_err(|e| match e {
        rmp::decode::MarkerReadError(_) => ReplCodecError::Truncated(format!("at {at} (marker)")),
    })?;
    match marker {
        Marker::Null => Ok(None),
        Marker::Bin8 => {
            let n = read_one_byte(rd, at)? as usize;
            Ok(Some(read_bin_body(rd, n, at)?))
        }
        Marker::Bin16 => {
            let n = read_be_u16(rd, at)? as usize;
            Ok(Some(read_bin_body(rd, n, at)?))
        }
        Marker::Bin32 => {
            let n = read_be_u32(rd, at)? as usize;
            Ok(Some(read_bin_body(rd, n, at)?))
        }
        other => Err(ReplCodecError::Type {
            at,
            detail: format!("expected bin or nil, got marker {other:?}"),
        }),
    }
}

fn read_bin_body(rd: &mut &[u8], n: usize, at: &'static str) -> Result<Arc<[u8]>, ReplCodecError> {
    let raw = take(rd, n, at)?;
    Ok(Arc::from(raw))
}

/// Split `n` bytes off the front of the cursor, advancing it. Errors (does not
/// panic) if fewer than `n` remain.
fn take<'a>(rd: &mut &'a [u8], n: usize, at: &'static str) -> Result<&'a [u8], ReplCodecError> {
    if rd.len() < n {
        return Err(ReplCodecError::Truncated(format!(
            "at {at}: need {n} bytes, have {}",
            rd.len()
        )));
    }
    let (head, tail) = rd.split_at(n);
    *rd = tail;
    Ok(head)
}

fn read_one_byte(rd: &mut &[u8], at: &'static str) -> Result<u8, ReplCodecError> {
    Ok(take(rd, 1, at)?[0])
}

fn read_be_u16(rd: &mut &[u8], at: &'static str) -> Result<u16, ReplCodecError> {
    let b = take(rd, 2, at)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn read_be_u32(rd: &mut &[u8], at: &'static str) -> Result<u32, ReplCodecError> {
    let b = take(rd, 4, at)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
