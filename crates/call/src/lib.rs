//! `call` — the CallContext data model.
//!
//! A pure, synchronous leaf crate: the [`Call`]→[`model::Leg`]→[`model::Dialog`]
//! data model ([`model`]), its lens/accessor/timer helpers ([`helpers`]),
//! `callRef` derivation ([`callref`]), the call [`incarnation`] grammar, the
//! SIP routing index grammar ([`index_key`]), decision-engine feature
//! activations ([`features`]), the header statements a decision makes
//! ([`header_update`]), and the pluggable body [`codec`].
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
pub mod header_update;
pub mod helpers;
pub mod incarnation;
pub mod index_key;
pub mod model;

// callRef derivation + parsing (a-leg identity → replicated key).
pub use callref::{call_ref_primary, derive_call_ref, parse_call_ref, ParsedCallRef};
// Which of the successive calls on one callRef: `{call_ref}#{mark}`.
pub use incarnation::{derive_incarnation, incarnation_mark};
// The SIP routing index: a call's keys and the lookups that read them.
pub use index_key::{
    call_index_keys, call_index_keys_from_unknown, IndexHit, IndexLookup, KeyKind, Probe,
};
// Pluggable body codec (msgpack default).
pub use codec::{CallBodyCodec, CallDecodeError, MsgpackCodec};
// The Call→Leg→Dialog tree + call-level satellites.
pub use model::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, Call, CallModelState, CallTopology, Leg,
    PolicyUpdateBody, PrackedProvisional, ReliableProvisional, SipHeader, TagMapping,
};
// The call's admission state on the call limiter (ADR-0040).
pub use model::{
    AdmitOutcome, AdmitReport, CallLimiterState, LimiterEntry, LimiterHeld, RefreshApplied,
    Replacement, CHANGE_EPOCH,
};
// Leg + dialog state.
pub use model::{
    B2buaDialogExt, ByeDisposition, Dialog, Direction, HostPort, InviteTxnHandle, LegDisposition,
    LegKind, LegSdpSession, LegState, PendingRequest, RemoteInfo, StackDialog, Unacked2xx,
};
// The retained emission every dialog-level retransmission repeats (ADR-0032 X3)
// and the obligation that discharges its ladder (X4).
pub use model::{Obligation, Repeat, Repeated, RetainedEmission};
// Timers, CDR events, the message ring, the decision log and the termination
// record on the replicated body.
pub use model::{
    CdrEvent, CdrEventType, DecisionKind, DecisionMark, MessageDirection, MessageEntry,
    MessageRing, Termination, TerminationCause, TimeoutKind, TimerEntry, TimerType,
};
// Per-service slices + state-machine identifiers (ADR-0016).
pub use model::{
    ExtMap, MachineId, PromotePemState, RelayFirst18xState, ReleaseEventKind, ReroutePhase,
    RerouteState, StateLabel, TransferPhase, TransferState,
};
