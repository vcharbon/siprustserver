//! Pluggable call-body codec.
//!
//! [`CallBodyCodec`] is the DI seam; [`MsgpackCodec`] is the one production
//! impl: `rmp-serde`'s **default positional/array** encoding, which keeps field
//! names off the wire (smallest payload + least CPU). The schema-coupling that
//! buys — field order is the contract — is acceptable because the project
//! redeploys from scratch each release (ADR-0008).

use serde::Deserialize;

use crate::model::Call;

/// The deepest nesting of arrays and maps a body may hold: the call tree's
/// own, with room for the JSON of the per-service `ext` slices. A peer's body
/// nested deeper is refused before its recursion can exhaust the stack.
pub const MAX_DEPTH: usize = 64;

/// Failure decoding a call body.
#[derive(Debug, thiserror::Error)]
pub enum CallDecodeError {
    /// Empty input (paranoid-decode precondition).
    #[error("decode received an empty buffer")]
    Empty,
    /// The bytes were not a valid encoded [`Call`], or nested past
    /// [`MAX_DEPTH`].
    #[error("msgpack decode failed: {0}")]
    Decode(String),
}

/// Failure encoding a call body.
#[derive(Debug, thiserror::Error)]
#[error("msgpack encode failed: {0}")]
pub struct CallEncodeError(String);

/// Encode/decode a [`Call`] to/from a self-contained body. The DI seam: a later
/// protobuf impl slots in behind the same trait.
pub trait CallBodyCodec {
    /// Pack a call for storage. Takes `&Call`, so it cannot mutate its input
    /// (the P10/P11 no-mutation codec properties hold by construction).
    fn encode(&self, call: &Call) -> Result<Vec<u8>, CallEncodeError>;
    /// Unpack a previously-encoded body.
    fn decode(&self, bytes: &[u8]) -> Result<Call, CallDecodeError>;
}

/// `rmp-serde` positional-encoding codec.
#[derive(Clone, Copy, Debug, Default)]
pub struct MsgpackCodec;

impl MsgpackCodec {
    pub fn new() -> Self {
        MsgpackCodec
    }
}

impl CallBodyCodec for MsgpackCodec {
    fn encode(&self, call: &Call) -> Result<Vec<u8>, CallEncodeError> {
        rmp_serde::to_vec(call).map_err(|e| CallEncodeError(e.to_string()))
    }

    fn decode(&self, bytes: &[u8]) -> Result<Call, CallDecodeError> {
        if bytes.is_empty() {
            return Err(CallDecodeError::Empty);
        }
        let mut de = rmp_serde::Deserializer::from_read_ref(bytes);
        de.set_max_depth(MAX_DEPTH);
        Call::deserialize(&mut de).map_err(|e| CallDecodeError::Decode(e.to_string()))
    }
}
