//! `call` — the CallContext data model (slice "CallContext data model" of the
//! migration; port of the pure half of `src/call/`).
//!
//! A pure, synchronous leaf crate: the [`Call`]→[`Leg`]→[`Dialog`] data model
//! ([`model`]), its lens/accessor/timer helpers ([`helpers`]), `callRef` +
//! index-key derivation ([`callref`]), and the pluggable body [`codec`].
//!
//! ## What this crate is NOT (deferred — see MIGRATION_STATUS + ADR-0008)
//!
//! - **`CallState`** — the stateful, per-call-serialized owner of the in-memory
//!   call map + Redis persistence + orphan sweep + HA topology. It depends on
//!   infrastructure not yet ported (cache, `AppConfig`, `CdrWriter`, metrics,
//!   the call limiter, the per-call dispatcher).
//! - **`TimerService`** — live timer scheduling. The data model carries only
//!   *serializable* [`model::TimerEntry`] intents; when CallState lands, firing
//!   rides `sip-txn`'s `DelayQueue` driver, not a new wheel.
//! - **The contract decorator wrappers** (`paranoidInputs`/`parity`/
//!   `scopedAudit`). Both production codecs are present: the msgpack
//!   ([`codec::MsgpackCodec`]) and protobuf ([`codec::ProtobufCodec`]) impls
//!   behind the [`codec::CallBodyCodec`] seam, the latter mapping
//!   [`model::Call`] to/from [`proto::wire::Call`] (the [`proto`] codegen of the
//!   TS `call.proto.gen.cjs`). The `parity` cross-codec comparison decorator is
//!   the remaining deferred piece; per-codec property checks live in the test
//!   suite.

pub mod callref;
pub mod codec;
pub mod features;
pub mod helpers;
pub mod model;
pub mod proto;

pub use callref::{
    call_index_keys, call_index_keys_from_unknown, call_ref_primary, derive_call_ref, parse_call_ref,
    ParsedCallRef,
};
pub use codec::{CallBodyCodec, CallDecodeError, MsgpackCodec, ProtobufCodec};
pub use model::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, B2buaDialogExt, ByeDisposition, Call,
    CallLimiterState, CallModelState, CallTopology, CdrEvent, CdrEventType, Dialog, Direction,
    ExtMap, HostPort, InviteTxnHandle, Leg, LegDisposition, LegKind, LegState, MachineId,
    PendingRequest, PolicyUpdateBody, PromotePemState, RelayFirst18xState, RemoteInfo, SipHeader,
    StackDialog, StateLabel, TagMapping,
    TimerEntry, TimerType, TransferPhase, TransferState,
};
