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
//! - **The protobuf codec impl** and the **contract decorator wrappers**
//!   (`paranoidInputs`/`parity`/`scopedAudit`). The protobuf *wire schema +
//!   codegen toolchain* lands here ([`proto`], the Rust analogue of the TS
//!   `call.proto.gen.cjs`); the `CallBodyCodec` impl that maps [`model::Call`]
//!   to/from [`proto::wire::Call`] is a separate stacked item. The
//!   [`codec::CallBodyCodec`] trait keeps the seam; property checks live in the
//!   test suite.

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
pub use codec::{CallBodyCodec, CallDecodeError, MsgpackCodec};
pub use model::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, B2buaDialogExt, ByeDisposition, Call,
    CallLimiterState, CallModelState, CallTopology, CdrEvent, CdrEventType, Dialog, Direction,
    ExtMap, HostPort, InviteTxnHandle, Leg, LegDisposition, LegKind, LegState, MachineId,
    PendingRequest, PolicyUpdateBody, PromotePemState, RelayFirst18xState, RemoteInfo, SipHeader,
    StackDialog, StateLabel, TagMapping,
    TimerEntry, TimerType, TransferPhase, TransferState,
};
