//! Pluggable call-body codec.
//!
//! [`CallBodyCodec`] is the DI seam; [`MsgpackCodec`] is the one production
//! impl: `rmp-serde`'s **default positional/array** encoding, which keeps field
//! names off the wire (smallest payload + least CPU). The schema-coupling that
//! buys — field order is the contract — is acceptable because the project
//! redeploys from scratch each release (ADR-0008).

use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::model::Call;

/// The nesting of arrays and maps a body stays under, on both sides: the call
/// tree's own, with room for [`EXT_JSON_DEPTH`] levels of per-service JSON. A
/// peer's body nested deeper is refused before its recursion can exhaust the
/// stack, and this node never encodes one a peer would refuse.
pub const MAX_DEPTH: usize = 64;

/// How deep the JSON of an `ext` slice may nest: the call tree takes the rest
/// of [`MAX_DEPTH`].
pub const EXT_JSON_DEPTH: usize = 48;

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
pub enum CallEncodeError {
    /// The serializer refused the call.
    #[error("msgpack encode failed: {0}")]
    Encode(String),
    /// The body nests past [`MAX_DEPTH`]: no peer would decode it.
    #[error("the body nests past {MAX_DEPTH} levels")]
    TooDeep,
}

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
        let bytes = rmp_serde::to_vec(call).map_err(|e| CallEncodeError::Encode(e.to_string()))?;
        // The decoder's own bound, walked over the body without building it.
        IgnoredAny::deserialize(&mut bounded(&bytes)).map_err(|_| CallEncodeError::TooDeep)?;
        Ok(bytes)
    }

    fn decode(&self, bytes: &[u8]) -> Result<Call, CallDecodeError> {
        if bytes.is_empty() {
            return Err(CallDecodeError::Empty);
        }
        Call::deserialize(&mut bounded(bytes)).map_err(|e| CallDecodeError::Decode(e.to_string()))
    }
}

/// A decoder over `bytes` that refuses nesting past [`MAX_DEPTH`].
fn bounded(bytes: &[u8]) -> rmp_serde::Deserializer<rmp_serde::decode::ReadRefReader<'_, [u8]>> {
    let mut de = rmp_serde::Deserializer::from_read_ref(bytes);
    de.set_max_depth(MAX_DEPTH);
    de
}
