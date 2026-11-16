//! The facts a reactor folds into the observed state ([`Observation`]); the
//! fold itself is [`super::state`]'s.

use super::facts::{RequestFact, ResponseFact};
use super::ledger::{CseqStreamKey, ObligationKey};
use super::state::SubflowState;
use crate::realcall::LostMessage;

/// A single fact a reactor folds into the observed state. Each variant is a
/// monotone, idempotent, commutative update (see the module doc), so a
/// double-observation is harmless and the fold is order-independent.
#[derive(Debug, Clone)]
pub enum Observation {
    /// An early dialog exists on `leg` — our INVITE drew an 18x, or we sent a
    /// 180 for a received INVITE. Never demotes a `Confirmed`/`Terminated` leg.
    LegEarly { leg: &'static str },
    /// `leg`'s dialog is confirmed — our INVITE's 2xx was ACKed, or the peer
    /// ACKed our 2xx.
    LegConfirmed { leg: &'static str },
    /// `leg` reached a terminal state — a BYE/200, a non-2xx final, a CANCEL.
    LegTerminated { leg: &'static str },
    /// `leg`'s DIALOG is being torn down (a BYE in either direction) — discharge
    /// its still-open in-dialog acknowledgement obligations (a pending re-INVITE
    /// ACK / PRACK-200 / … whose peer transaction dies with the call can never
    /// arrive), so the settle barrier does not hold the verdict its 32 s ceiling
    /// for an impossible ack. See [`ObligationLedger::discharge_leg`].
    DialogTornDown { leg: &'static str },
    /// We answered a received dialog-creating INVITE — seed its stream (sender
    /// tag: the INVITE's From-tag) with the INVITE's CSeq, so the first
    /// in-dialog request is not a phantom hole (§12.2.1.1).
    SeedDialog { stream: CseqStreamKey, cseq: u32 },
    /// We sent an in-dialog request awaiting its final — opens a ledger
    /// obligation.
    RequestSent { key: ObligationKey, detail: String },
    /// A final response to one of our sent in-dialog requests — closes the
    /// obligation.
    ResponseObserved { key: ObligationKey },
    /// An in-dialog request arrived on `stream`'s dialog — folds into its CSeq
    /// gap detector (any method) AND into the receiving leg's received-method
    /// set (so an origination can gate on "leg X received an inbound `<method>`").
    InDialogRequest { stream: CseqStreamKey, cseq: u32, method: String },
    /// An in-dialog request the dialog table refused still arrived: its CSeq is no hole.
    RefusedInDialog { stream: CseqStreamKey, cseq: u32 },
    /// What an in-dialog request `leg` received said, once per `(call_id, cseq, method)`.
    LegRequest { leg: &'static str, call_id: String, cseq: u32, fact: RequestFact },
    /// `leg`'s establishing INVITE drew a non-2xx final (the reject path) —
    /// grow-only, read by the accepted-ending verdict.
    LegFinal { leg: &'static str, status: u16, reason: String },
    /// The caller's establishing INVITE drew a final its call accepts: the call
    /// ended before its remaining phase barriers could hold.
    AcceptedEnding,
    /// The system under test ended a party cleanly while the call accepts
    /// clean ends ([`CallCtx::accept_clean_ends`](crate::realcall::CallCtx::accept_clean_ends));
    /// `caller` when that party originates the call.
    EndedCleanly { caller: bool },
    /// A party the system under test ended cleanly dropped a goal the ending
    /// did not itself meet.
    GoalDropped,
    /// `leg` sent a CANCEL for its establishing INVITE.
    CancelSent { leg: &'static str },
    /// A named sub-flow advanced (re-INVITE realign / refer / early-PRACK tracking).
    Subflow { leg: &'static str, name: &'static str, to: SubflowState },
    /// A renegotiation this leg ORIGINATED on `dialog` (its local and remote
    /// tags) completed — a re-INVITE's 2xx received and ACKed, an UPDATE's
    /// 2xx received. Folds into the leg's grow-only completed-reneg set (keyed
    /// by dialog and CSeq, so a re-emitted 2xx cannot double-count), whose
    /// cardinality serializes an N-cycle re-INVITE script.
    RenegCompleted { leg: &'static str, dialog: (String, String), cseq: u32 },
    /// An inbound response observed on `leg` — appended to the leg's ordered
    /// response log (one writer: the leg's own reactor), which reception goals
    /// consume through a per-actor cursor.
    LegResponse { leg: &'static str, fact: ResponseFact },
    /// An `ObserveFinal`/`ExpectFinal` outcome — appended to the replay record.
    ReplayFinal { key: u32, expected: Option<u16>, observed: u16 },
    /// A request the reactor (not the script) serviced on a `Scripted` actor —
    /// appended to the replay record so divergence is never silent.
    ServicedStray { leg: &'static str, method: String, action: &'static str },
    /// An accepted-delta policy blessed a non-matching inbound as satisfying
    /// the due scripted expectation (ADR-0024 §6) — appended to the replay
    /// record so an acceptance is never silent.
    AcceptedDelta {
        leg: &'static str,
        step: usize,
        expected: String,
        observed: String,
        rule: &'static str,
    },
    /// A shared endpoint's demux found no actor for an inbound (see
    /// [`ReplayEntry::UnclaimedInbound`]).
    UnclaimedInbound { endpoint: String, detail: String },
    /// A final the peer retransmitted while `leg` held its ACK for it (see
    /// [`ReplayEntry::HeldFinalRetransmitted`]).
    HeldFinalRetransmitted { leg: &'static str, status: u16, cseq_method: String, cseq: u32 },
    /// The call's loss model dropped a datagram to or from `leg`.
    Lost { leg: &'static str, message: LostMessage },
    /// A reception goal's body expectation missed (see
    /// [`ReplayEntry::BodyExpectMiss`]) — appended to the replay record so the
    /// miss is never silent.
    BodyExpectMiss {
        leg: &'static str,
        step: usize,
        expected: &'static str,
        body_len: usize,
        body_is_sdp: bool,
        status: Option<u16>,
        method: String,
        initial: bool,
    },
}
