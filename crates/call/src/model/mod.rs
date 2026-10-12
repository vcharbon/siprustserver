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
//!     state, topology, peering, INVITE snapshot, policy)
//!   - [`limiter`] — the call's admission state on the call limiter
//!   - [`leg`] — [`Leg`] + state / disposition / role enums
//!   - [`attempt`] — one routing answer the call dials and its legs
//!   - [`dialog`] — one §12 dialog: identity, state, sequences, what the
//!     B2BUA keeps on it
//!   - [`dialogs`] — a leg's dialogs and the one lookup surface over them
//!   - [`outbound_invite`] — the dialog-creating INVITE a leg sent
//!   - [`emission`] / [`obligation`] — the retained emission a ladder repeats
//!     and the key that discharges it (ADR-0032)
//!   - [`provisional`] — one reliable provisional the call received (RFC 3262)
//!   - [`owed_answer`] — the offer a dialog's responder awaits an answer to
//!   - [`invite_txn`] — in-flight INVITE client-transaction handle
//!   - [`timer`] — serializable timer intents ([`TimerType`], [`TimerScope`],
//!     [`TimerKey`], [`TimerEntry`])
//!   - [`deadline`] — a configured wait that may be off ([`Deadline`])
//!   - [`cdr`] — CDR event records
//!   - [`decision_log`] — the applied decisions, in order
//!   - [`message_ring`] — the capped per-leg history of distinct SIP messages
//!   - [`termination`] — who ended the call and why, and the ring cut
//!   - [`services`] — per-service typed runtime slices + opaque [`ExtMap`]
//!   - [`sm`] — state-machine identifiers (ADR-0016)

pub mod attempt;
pub mod cdr;
pub mod deadline;
pub mod decision_log;
pub mod dialog;
pub mod dialogs;
pub mod emission;
pub mod invite_txn;
pub mod leg;
pub mod leg_state;
pub mod limiter;
pub mod message_ring;
pub mod obligation;
pub mod outbound_invite;
pub mod owed_answer;
pub mod provisional;
pub mod record;
pub mod sdp_session;
pub mod services;
pub mod sm;
pub mod termination;
pub mod timer;

pub use attempt::{Attempt, AttemptCause, AttemptId, AttemptOutcome};
pub use cdr::{CdrEvent, CdrEventType};
pub use deadline::Deadline;
pub use decision_log::{DecisionKind, DecisionMark};
pub use dialog::{
    Dialog, DialogEnd, DialogId, DialogState, Direction, EndingBy, PeerDialog, PendingRequest,
    RetiredDialog, StragglerRelease, Unacked2xx,
};
pub use dialogs::Dialogs;
pub use emission::{Repeat, Repeated, RetainedEmission};
pub use invite_txn::{HostPort, InviteTxnHandle};
pub use leg::{Leg, LegKind, RemoteInfo, Ring};
pub use leg_state::{ByeDisposition, LegDisposition, LegState};
pub use limiter::{
    AdmitOutcome, AdmitReport, CallLimiterState, LimiterEntry, LimiterHeld, RefreshApplied,
    Replacement, CHANGE_EPOCH,
};
pub use message_ring::{MessageDirection, MessageEntry, MessageRing};
pub use obligation::{Obligation, ObligationKind};
pub use outbound_invite::OutboundInvite;
pub use owed_answer::OwedAnswer;
pub use provisional::{
    CSeq, Ceased, Ladder, Provisional, ProvisionalId, RSeq, ResponderAck, Shown,
};
pub use record::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, Call, CallModelState, CallTopology,
    PolicyUpdateBody, SipHeader,
};
pub use sdp_session::LegSdpSession;
pub use services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, ReroutePhase, RerouteState,
    TransferPhase, TransferState,
};
pub use sm::{MachineId, StateLabel};
pub use termination::{Termination, TerminationCause, TimeoutKind};
pub use timer::{TimerEntry, TimerKey, TimerScope, TimerScopeKind, TimerType};
