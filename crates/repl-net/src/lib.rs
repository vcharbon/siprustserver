//! `repl-net` — the HA replication **wire + transport layer** (slices S2–S3).
//!
//! This crate is the call-agnostic frame model + codec + framing (S2) plus the
//! transport seam (S3) that the peer-to-peer replication engine (ADR-0011)
//! speaks over. The S2 wire layer is the replication analogue of `sip-message`:
//! pure, synchronous, no transport. The transport seam
//! ([`ReplicationNetwork`][transport::ReplicationNetwork]: sim + real TCP +
//! recording) is the analogue of `sip-net`'s `SignalingNetwork`, but for a
//! reliable, ordered, message-granular framed stream (Decision X2).
//!
//! ## What is here
//! - [`Frame`] — the five positional-msgpack replication messages
//!   ([`Frame::PullRequest`], [`Frame::Ack`], [`Frame::Data`], [`Frame::Noop`],
//!   [`Frame::ResetToBootstrap`]) plus the [`PullMode`] / [`Op`] /
//!   [`Partition`] value enums and the [`Watermark`] ordering.
//! - [`encode_frame`] / [`decode_frame`] — exact positional-msgpack codec for a
//!   single frame (`[tag, ...]` array; field order is the contract, ADR-0008).
//! - [`frame_with_len_prefix`] / [`try_read_framed`] — the 4-byte BE
//!   length-prefix framing the real TCP transport (S3) will delimit with.
//!
//! ## Call-agnostic by design
//! A [`Frame::Data`] body is opaque bytes ([`std::sync::Arc<[u8]>`]) — the
//! encoded `Call` is read straight from the store and forwarded verbatim; this
//! layer never decodes it (ADR-0011 X9, Decision 9). That keeps the wire layer
//! decoupled from `crates/call`.
//!
//! ## Positional-msgpack ethos (ADR-0008)
//! Each frame is a msgpack ARRAY with the integer tag at element 0 and no field
//! names on the wire — smallest payload, least CPU, field ORDER is the contract.
//! The codec is hand-written over the low-level `rmp` crate (not serde-derived)
//! for exact positional control. Some flat wire elements are grouped behind a
//! [`Watermark`] in Rust (e.g. `PullRequest`'s `since_gen` + `since_counter`);
//! they always serialise back to two flat array elements.

pub mod codec;
pub mod frame;
pub mod framing;
pub mod transport;

pub use codec::{decode_frame, encode_frame, ReplCodecError};
pub use frame::{Frame, Op, Partition, PullMode, UnknownDiscriminant, Watermark};
pub use framing::{frame_with_len_prefix, try_read_framed, ReplFramingError, MAX_FRAME_LEN};
pub use transport::{
    CapturedFrame, ConnectError, Direction, Fault, ListenError, RealReplicationNetwork,
    RecordingReplicationNetwork, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError, SimulatedReplicationNetwork,
};
