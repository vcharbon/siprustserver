//! Pluggable call-body codec — port of `src/call/codec/` (the `CallBodyCodec`
//! tag + the msgpack impl) and `CallCodec.ts`.
//!
//! [`CallBodyCodec`] is the DI seam (the Rust analogue of the source's Effect
//! `CallBodyCodec` tag). Two impls slot behind it:
//!
//! - [`MsgpackCodec`] — `rmp-serde`'s **default positional/array** encoding,
//!   which keeps field names off the wire (smallest payload + least CPU). The
//!   schema-coupling that buys — field order is the contract — is acceptable
//!   because the project redeploys from scratch each release.
//! - [`ProtobufCodec`] — encodes against the `proto/call.proto` wire schema (the
//!   [`crate::proto`] codegen) via the `model::Call` ↔ `proto::wire::Call`
//!   mapping shims in [`protobuf`]. The Rust analogue of the source's
//!   `ProtobufLayer` (`src/call/codec/protobuf.ts`). A codec swap is a
//!   fresh-cluster event (ADR-0011), so the two are never mixed on the wire.
//!
//! The parity comparison wrapper (the source's `parity` decorator) is still
//! deferred (see ADR-0008 / MIGRATION_STATUS).

mod protobuf;

use prost::Message;

use crate::model::Call;
use crate::proto::wire;

/// Failure decoding a call body.
#[derive(Debug, thiserror::Error)]
pub enum CallDecodeError {
    /// Empty input (the source's PA2 paranoid-decode precondition).
    #[error("decode received an empty buffer")]
    Empty,
    /// The bytes were not a valid encoded [`Call`] — for [`MsgpackCodec`] a bad
    /// msgpack frame; for [`ProtobufCodec`] a `prost` wire-decode failure, a bad
    /// closed-union token, a missing structurally-required submessage, or a
    /// malformed JSON side-channel carry.
    #[error("call-body decode failed: {0}")]
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

/// Protobuf codec — encodes a [`Call`] against the `proto/call.proto` wire schema
/// (the Rust analogue of the source's `ProtobufLayer`). [`encode`](Self::encode)
/// maps the model into [`proto::wire::Call`](crate::proto::wire::Call) and
/// `prost`-encodes it; [`decode`](Self::decode) reverses both steps. The
/// `model::Call` ↔ `wire::Call` mapping — the `*IsNull` / `*Present` side-channels
/// and the JSON-string carries (`featuresJson`, `extJson`, `pendingInviteTxnJson`,
/// …) — lives in the [`protobuf`] submodule.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProtobufCodec;

impl ProtobufCodec {
    pub fn new() -> Self {
        ProtobufCodec
    }
}

impl CallBodyCodec for ProtobufCodec {
    fn encode(&self, call: &Call) -> Vec<u8> {
        // The mapping is total (every model distinction has a wire home) and
        // proto3 encode is infallible, so this never errors — matching the
        // source `encode` (which returns a `Buffer`, not an `Effect`).
        protobuf::to_proto(call).encode_to_vec()
    }

    fn decode(&self, bytes: &[u8]) -> Result<Call, CallDecodeError> {
        // PA2: an empty buffer is a precondition violation, not an empty `Call`.
        // (A zero-length proto body decodes to an all-default message, which
        // would then fail `from_proto` on the missing required `aLeg`; rejecting
        // here keeps parity with the source's paranoid-decode guard and the
        // msgpack codec's `Empty`.)
        if bytes.is_empty() {
            return Err(CallDecodeError::Empty);
        }
        let wire = wire::Call::decode(bytes)
            .map_err(|e| CallDecodeError::Decode(format!("protobuf wire decode failed: {e}")))?;
        protobuf::from_proto(wire)
    }
}
