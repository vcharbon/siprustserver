//! Pluggable call-body codec — port of `src/call/codec/` (the `CallBodyCodec`
//! tag + the msgpack impl) and `CallCodec.ts`.
//!
//! [`CallBodyCodec`] is the DI seam (the Rust analogue of the source's Effect
//! `CallBodyCodec` tag). [`MsgpackCodec`] is the one production impl in this
//! slice: `rmp-serde`'s **default positional/array** encoding, which keeps field
//! names off the wire (smallest payload + least CPU). The schema-coupling that
//! buys — field order is the contract — is acceptable because the project
//! redeploys from scratch each release. The protobuf codec and the parity
//! comparison are deferred (see ADR-0008 / MIGRATION_STATUS).

use crate::model::Call;

/// Failure decoding a call body.
#[derive(Debug, thiserror::Error)]
pub enum CallDecodeError {
    /// Empty input (the source's PA2 paranoid-decode precondition).
    #[error("decode received an empty buffer")]
    Empty,
    /// The bytes were not a valid encoded [`Call`].
    #[error("msgpack decode failed: {0}")]
    Decode(String),
}

/// Encode/decode a [`Call`] to/from a self-contained body. The DI seam: a later
/// protobuf impl slots in behind the same trait.
pub trait CallBodyCodec {
    /// Pack a call for storage. Takes `&Call`, so it cannot mutate its input
    /// (the source's P10/P11 no-mutation properties hold by construction).
    fn encode(&self, call: &Call) -> Vec<u8>;
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
    fn encode(&self, call: &Call) -> Vec<u8> {
        // The Call tree contains no non-string map keys and no unsupported
        // types, so serialization is infallible.
        rmp_serde::to_vec(call).expect("Call serialization is infallible")
    }

    fn decode(&self, bytes: &[u8]) -> Result<Call, CallDecodeError> {
        if bytes.is_empty() {
            return Err(CallDecodeError::Empty);
        }
        rmp_serde::from_slice(bytes).map_err(|e| CallDecodeError::Decode(e.to_string()))
    }
}
