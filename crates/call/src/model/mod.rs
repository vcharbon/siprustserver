//! The B2BUA call data model. Three-level hierarchy: [`Call`] → [`Leg`] →
//! [`Dialog`]. The whole tree is a pure, serializable value (no I/O, no
//! runtime) that round-trips through the body codec ([`crate::codec`]) for
//! persistence/replication. Lenses and accessors do NOT live here — see
//! [`crate::helpers`].
//!
//! ## Encoding contracts
//!
//! - **Optional vs null.** Absent and nil collapse to `Option<T>` under
//!   msgpack. The one field where a *three-way* absent/null/value distinction
//!   is behaviourally load-bearing — `Call::policy_update_body` — is preserved
//!   as [`PolicyUpdateBody`] (`None` = no override, `Some(Empty)` = force
//!   empty body, `Some(Bytes)` = substitute).
//! - **Opaque `ext`.** Per-service slices are carried verbatim as [`ExtMap`]
//!   (`serde_json::Value`); core never interprets them.
//! - **Maps are `BTreeMap`** (not `HashMap`) so encode is deterministic
//!   (codec property P2).
//! - **Byte fields** (`a_leg_invite.body`, `cached_sdp`, INVITE handle) use
//!   `serde_bytes` so msgpack stores them as `bin`; SIP payloads stay raw
//!   bytes — the data model takes no `sip-message` dep.
//!
//! Concern map:
//!   - [`record`] — master [`Call`] record + call-level satellites (lifecycle
//!     state, topology, peering, limiters, INVITE snapshot, tag map, policy)
//!   - [`leg`] — [`Leg`] + state / disposition / role enums
//!   - [`dialog`] — §12 dialog state + B2BUA-only dialog extensions
//!   - [`emission`] / [`obligation`] — the retained emission a ladder repeats
//!     and the key that discharges it (ADR-0032)
//!   - [`invite_txn`] — in-flight INVITE client-transaction handle
//!   - [`timer`] — serializable timer intents ([`TimerType`] / [`TimerEntry`])
//!   - [`cdr`] — CDR event records
//!   - [`services`] — per-service typed runtime slices + opaque [`ExtMap`]
//!   - [`sm`] — state-machine identifiers (ADR-0016)

pub mod cdr;
pub mod dialog;
pub mod emission;
pub mod invite_txn;
pub mod leg;
pub mod obligation;
pub mod record;
pub mod services;
pub mod sm;
pub mod timer;

pub use cdr::{CdrEvent, CdrEventType};
pub use dialog::{B2buaDialogExt, Dialog, Direction, PendingRequest, StackDialog, Unacked2xx};
pub use emission::{Repeat, Repeated, RetainedEmission};
pub use invite_txn::{HostPort, InviteTxnHandle};
pub use leg::{ByeDisposition, Leg, LegDisposition, LegKind, LegState, RemoteInfo};
pub use obligation::Obligation;
pub use record::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, Call, CallLimiterState, CallModelState,
    CallTopology, PolicyUpdateBody, PrackedProvisional, ReliableProvisional, SipHeader, TagMapping,
};
pub use services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, ReroutePhase, RerouteState,
    TransferPhase, TransferState,
};
pub use sm::{MachineId, StateLabel};
pub use timer::{TimerEntry, TimerType};
