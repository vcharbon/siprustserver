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
//! - **Byte fields** (`a_leg_invite.body`, `cached_sdp`, a sent request's image) use
//!   `serde_bytes` so msgpack stores them as `bin`; SIP payloads stay raw
//!   bytes — the data model takes no `sip-message` dep.
//!
//! Concern map:
//!   - [`record`] — master [`Call`] record + call-level satellites (lifecycle
//!     state, peering, INVITE snapshot, policy)
//!   - [`limiter`] — the call's admission state on the call limiter
//!   - [`leg`] — [`Leg`] + state / disposition / role enums
//!   - [`attempt`] — one routing answer the call dials and its legs
//!   - [`dialog`] — one §12 dialog: identity, state, sequences, what the
//!     B2BUA keeps on it
//!   - [`dialogs`] — a leg's dialogs and the one lookup surface over them
//!   - [`request_record`] — one request of the call still open: relayed, own,
//!     held for a later answer or answered (ADR-0055 §2)
//!   - [`invite_exchange`] — what an INVITE's record holds beyond its final:
//!     the 2xx taken or sent and its ACK
//!   - [`outbound_invite`] — the dialling facts of the INVITE a leg sent
//!   - [`leg_invite`] — a leg's dialog-creating INVITE read off its record
//!   - [`emission`] / [`obligation`] — the retained emission a ladder repeats
//!     and the key that discharges it (ADR-0032)
//!   - [`provisional`] — one reliable provisional the call received (RFC 3262)
//!   - [`owed_answer`] — the offer a dialog's responder awaits an answer to
//!   - [`subscription`] — a subscription this stack notifies on a dialog, its
//!     state and its NOTIFY awaiting a final (RFC 6665)
//!   - [`queued_request`] — a request of this stack waiting on a dialog
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
pub mod invite_exchange;
pub mod leg;
pub mod leg_invite;
pub mod leg_space;
pub mod leg_state;
pub mod limiter;
pub mod message_ring;
pub mod obligation;
pub mod outbound_invite;
pub mod owed_answer;
pub mod provisional;
pub mod queued_request;
pub mod record;
pub mod relay_owner;
pub mod request_record;
pub mod sdp_session;
pub mod services;
pub mod sm;
pub mod subscription;
pub mod termination;
pub mod timer;

pub use attempt::{Attempt, AttemptCause, AttemptId, AttemptOutcome};
pub use cdr::{CdrEvent, CdrEventType};
pub use deadline::{millis_i64, millis_u64, Deadline};
pub use decision_log::{DecisionKind, DecisionMark};
pub use dialog::{Dialog, DialogEnd, DialogId, DialogState, Direction, EndingBy, PeerDialog};
pub use dialogs::Dialogs;
pub use emission::{Repeat, Repeated, RetainedEmission};
pub use invite_exchange::{InviteExchange, Unacked2xx};
pub use leg::{Leg, LegKind, RemoteInfo, Ring};
pub use leg_space::LegSpace;
pub use leg_state::{ByeDisposition, LegDisposition, LegState};
pub use limiter::{
    AdmitOutcome, AdmitReport, CallLimiterState, ConsultAdmits, LimiterEntry, LimiterHeld,
    RefreshApplied, Replacement, CHANGE_EPOCH,
};
pub use message_ring::{MessageDirection, MessageEntry, MessageRing};
pub use obligation::{Obligation, ObligationKind};
pub use outbound_invite::OutboundInvite;
pub use owed_answer::OwedAnswer;
pub use provisional::{
    CSeq, Ceased, Ladder, Provisional, ProvisionalId, RSeq, ResponderAck, Shown,
};
pub use queued_request::{Behind, Queued, QueuedRequest};
pub use record::{
    ALegInviteSnapshot, ActivePeer, ActiveRule, Call, CallModelState, PolicyUpdateBody, SipHeader,
};
pub use relay_owner::RelayOwner;
pub use request_record::{
    DialogUsage, HostPort, RequestOrigin, RequestRecord, SentRequest, KEEPALIVE_AUTHOR,
};
pub use sdp_session::LegSdpSession;
pub use services::{
    ExtMap, PromotePemState, RelayFirst18xState, ReleaseEventKind, ReroutePhase, RerouteState,
    TransferPhase, TransferState,
};
pub use sm::{MachineId, StateLabel};
pub use subscription::{NotifyInFlight, Subscription, SubscriptionState};
pub use termination::{Termination, TerminationCause, TimeoutKind};
pub use timer::{TimerEntry, TimerKey, TimerScope, TimerScopeKind, TimerType};
