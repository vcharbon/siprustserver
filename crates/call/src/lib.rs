//! `call` — the CallContext data model.
//!
//! A pure, synchronous leaf crate: the [`Call`]→[`model::Leg`]→[`model::Dialog`]
//! data model ([`model`]), its lens/accessor/timer helpers ([`helpers`]),
//! `callRef` + index-key derivation ([`callref`]), decision-engine feature
//! activations ([`features`]), and the pluggable body [`codec`].
//!
//! ## What this crate is NOT (deferred — see ADR-0008)
//!
//! - **`CallState`** — the stateful, per-call-serialized owner of the in-memory
//!   call map + persistence + orphan sweep + HA topology; it lives with the
//!   b2bua runtime, not in this leaf.
//! - **`TimerService`** — live timer scheduling. The data model carries only
//!   *serializable* [`model::TimerEntry`] intents; firing rides `sip-txn`'s
//!   `DelayQueue` driver, not a new wheel.
//! - **SIP parsing.** SIP payloads are carried as raw bytes; header/message
//!   extraction lives in `sip-message` only.

pub mod callref;
pub mod codec;
pub mod features;
pub mod helpers;
pub mod model;

// callRef derivation + parsing (a-leg identity → replicated key).
pub use callref::{
    call_index_keys, call_index_keys_from_unknown, call_ref_primary, derive_call_ref, parse_call_ref,
    ParsedCallRef,
};
// Pluggable body codec (msgpack default).
pub use codec::{CallBodyCodec, CallDecodeError, MsgpackCodec};
// The Call→Leg→Dialog tree + call-level satellites.
pub use model::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, Call, CallLimiterState, CallModelState,
    CallTopology, Leg, PolicyUpdateBody, ReliableProvisional, SipHeader, TagMapping,
};
// Leg + dialog state.
pub use model::{
    B2buaDialogExt, ByeDisposition, Dialog, Direction, HostPort, InviteTxnHandle, LegDisposition,
    LegKind, LegState, PendingReinvite2xx, PendingRequest, RemoteInfo, StackDialog,
};
// Timers + CDR events on the replicated body.
pub use model::{CdrEvent, CdrEventType, TimerEntry, TimerType};
// Per-service slices + state-machine identifiers (ADR-0016).
pub use model::{
    ExtMap, MachineId, PromotePemState, RelayFirst18xState, ReleaseEventKind, ReroutePhase,
    RerouteState, StateLabel, TransferPhase, TransferState,
};
