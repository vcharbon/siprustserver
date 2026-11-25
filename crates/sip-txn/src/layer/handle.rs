//! Public surface: [`TransactionLayer`] (the clone-cheap handle), its
//! [`TransactionConfig`], and the [`Command`] and [`Forget`] funnels to the
//! owner task. Every mutation runs on the single writer: a command round-trips
//! over an mpsc + oneshot, a forget is one-way. The owner itself lives in `layer::owner`.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::UdpEndpoint;
use tokio::sync::{mpsc, oneshot};

use crate::event::{ClientTransactionHandle, TransactionEvent};
use crate::metrics::{MetricsInner, TransactionMetrics};
use crate::pulse::LoopPulse;
use crate::seed::{Reoffer, TxnSeed};
use call::TxnKind;

pub use super::config::TransactionConfig;
use super::event_age::EventAges;
pub use super::event_receiver::TransactionEvents;
use super::held_cancel::CancelledInvite;
use super::key::{ServerTxnId, ServerTxnKey};
use super::orphan::OrphanOrigin;
use super::owner::Owner;
use super::run::run;

mod command;
mod relinquish;

pub(super) use command::Command;

/// The command funnel's bound.
const COMMAND_CAPACITY: usize = 1024;

/// Forget the non-INVITE server transaction `key` names if it has sent nothing
/// yet (`Trying`) and holds the request's dialog identity; no reply. It rides
/// its own unbounded lane, which holds at most one per request the owner handed
/// up, so a stalled owner lets it grow by nothing it did not already emit.
pub(super) struct Forget {
    pub(super) key: ServerTxnKey,
    pub(super) call_id: String,
    pub(super) from_tag: String,
}

/// Handle to the running transaction layer. Clone-cheap; every method funnels
/// to the single owner task.
#[derive(Clone)]
pub struct TransactionLayer {
    cmd_tx: mpsc::Sender<Command>,
    forget_tx: mpsc::UnboundedSender<Forget>,
    metrics: TransactionMetrics,
    /// Aborts the owner task (and so drops the SIP endpoint it owns). Used to
    /// simulate a hard crash: the owner otherwise lives until every `cmd_tx` clone
    /// drops, which a surviving per-call task would keep alive — so a "crashed"
    /// node would keep answering SIP. Cheap to clone.
    owner_abort: tokio::task::AbortHandle,
    /// The owner loop's pulse: stamped from the instant a turn takes work
    /// until it waits again.
    owner_pulse: Arc<LoopPulse>,
}

impl TransactionLayer {
    /// Spawn the owner task over an already-bound endpoint. Returns the handle
    /// and the consumer's end of the bounded `events` queue and of the
    /// keepalive lane, when configured (the consumer — the proxy/B2BUA router —
    /// drains them). The task lives until both the returned handle (all
    /// clones) and the events receiver are dropped, or the endpoint closes.
    /// The owner task alone holds the endpoint; the metrics read its inbound
    /// queue through a weak handle, so aborting the owner drops the endpoint.
    pub fn spawn(
        endpoint: Box<dyn UdpEndpoint>,
        parser: Arc<dyn SipParser + Send + Sync>,
        config: TransactionConfig,
    ) -> (Self, TransactionEvents) {
        #[expect(clippy::disallowed_methods, reason = "event_capacity is within EVENT_CAPACITY")]
        let (events_tx, events_rx) = mpsc::channel::<TransactionEvent>(config.event_capacity());
        let ages = Arc::new(EventAges::new(config.event_capacity()));
        #[expect(clippy::disallowed_methods, reason = "the lane's bound is at least 1")]
        let (keepalive_tx, keepalive_rx) = config
            .keepalive_lane
            .map(|bound| mpsc::channel::<TransactionEvent>(bound.max(1)))
            .unzip();
        #[expect(clippy::disallowed_methods, reason = "the lane's bound is at least 1")]
        let (unattributed_tx, unattributed_rx) = config
            .unattributed_lane
            .map(|bound| mpsc::channel::<TransactionEvent>(bound.max(1)))
            .unzip();
        #[expect(clippy::disallowed_methods, reason = "COMMAND_CAPACITY is not zero")]
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_CAPACITY);
        let (forget_tx, forget_rx) = mpsc::unbounded_channel::<Forget>();

        let metrics_inner = Arc::new(MetricsInner::new());
        let endpoint: Arc<dyn UdpEndpoint> = Arc::from(endpoint);
        let metrics = TransactionMetrics::new(
            metrics_inner.clone(),
            events_tx.clone(),
            (ages.clone(), Arc::downgrade(&endpoint)),
            config.invite_refusals.clone(),
        );
        let events =
            TransactionEvents::new(events_rx, (keepalive_rx, unattributed_rx), ages.clone());

        let lanes = (keepalive_tx, unattributed_tx);
        let owner = Owner::new(parser, (events_tx, ages), lanes, metrics_inner, config);
        let owner_pulse = Arc::new(LoopPulse::new());
        let owner_abort =
            tokio::spawn(run(owner, endpoint, cmd_rx, forget_rx, owner_pulse.clone()))
                .abort_handle();

        (Self { cmd_tx, forget_tx, metrics, owner_abort, owner_pulse }, events)
    }

    pub fn metrics(&self) -> &TransactionMetrics {
        &self.metrics
    }

    /// The owner loop's pulse: how long its current turn has run, `None`
    /// while it waits for work.
    pub fn owner_pulse(&self) -> &Arc<LoopPulse> {
        &self.owner_pulse
    }

    /// The owner task, for a supervisor to see it end: every handle dropped,
    /// the endpoint closed, aborted, or panicked. The layer answers nothing
    /// after it. Holding it keeps neither the task nor the endpoint alive.
    pub fn owner_task(&self) -> tokio::task::AbortHandle {
        self.owner_abort.clone()
    }

    /// Abort the owner task — and so drop the SIP endpoint it owns, silencing the
    /// wire. For hard-crash simulation (the failover harness): without it the owner
    /// outlives `B2buaCore::abort` (a surviving per-call task still holds a `cmd_tx`
    /// clone, so `cmd_rx` never closes) and the "crashed" node keeps answering SIP
    /// — 100 Trying, 200/487 to CANCEL, cached-final replays, client retransmits.
    pub fn abort_owner(&self) {
        self.owner_abort.abort();
    }

    /// Funnel one command to the owner and await its oneshot reply. Returns
    /// [`TransactionLayerClosed`] instead of panicking when the owner task is gone
    /// (endpoint closed, or `abort_owner`) — the single funnel-and-error site for
    /// every public method.
    ///
    /// It completes within the owner turns queued ahead of it and its own: the
    /// owner awaits only sends that never suspend (ADR-0033 §5), and an owner
    /// stalled on a turn exits the worker (§4).
    async fn roundtrip<R>(
        &self,
        build: impl FnOnce(oneshot::Sender<R>) -> Command,
    ) -> Result<R, TransactionLayerClosed> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx.send(build(reply)).await.map_err(|_| TransactionLayerClosed)?;
        rx.await.map_err(|_| TransactionLayerClosed)
    }

    /// Send an outbound SIP request, allocating a client transaction (none
    /// for a CANCEL or an ACK) and returning its handle. An ACK of a 2xx is
    /// kept by its Accepted INVITE client transaction, which re-sends it to a
    /// repeat once the call is released. `Err` if the owner task is gone.
    pub async fn send_request(
        &self,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> Result<ClientTransactionHandle, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendRequest { msg: Box::new(msg), dest, txn_type, reply })
            .await
    }

    /// Send an outbound SIP response through its server transaction and
    /// return the datagram that left. The bytes are `msg.image()` verbatim
    /// unless the To-tag had to be bound to the transaction's (`bind_to_tag`),
    /// so a TU that retains an image for a repeat retains what is returned
    /// here (ADR-0032 X3). A response a server transaction here holds leaves
    /// to that transaction's destination, its request's response target
    /// (RFC 3261 §18.2.2); `dest` addresses any other.
    pub async fn send_response(
        &self,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Result<Bytes, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendResponse { msg: Box::new(msg), dest, reply }).await
    }

    /// Send `msg`, the final to a request the consumer found no call for, as
    /// a stateless UAS sends it (RFC 3261 §8.2.7), and return the datagram
    /// that left. The server transaction its request opened leaves with no
    /// trace: no copy record (ADR-0063 §1), no remembered To-tag, nothing on
    /// the unattributed books, so a copy of the request is a first arrival
    /// handed up again and the consumer, answering it the same, sends the same
    /// final. The response leaves once, raw, to that transaction's destination
    /// (§18.2.2), else to `dest`. A provisional, and a transaction that already
    /// sent a response or holds a CANCEL's answers, go through the transaction
    /// as [`send_response`](Self::send_response) sends them: an INVITE whose
    /// 100 left owes its final Timer G (§17.2.1).
    pub async fn send_stateless_response(
        &self,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Result<Bytes, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::SendStatelessResponse { msg: Box::new(msg), dest, reply })
            .await
    }

    /// Send a raw buffer directly, bypassing transaction management, counted
    /// under `call_id`, the message's Call-ID, against its budget
    /// ([`TransactionConfig::call_id_budget`]): nothing leaves under a muted
    /// one.
    pub async fn send_raw(
        &self,
        call_id: &str,
        buf: Vec<u8>,
        dest: SocketAddr,
    ) -> Result<(), TransactionLayerClosed> {
        let call_id = call_id.to_string();
        self.roundtrip(|reply| Command::SendRaw { call_id, buf, dest, reply }).await
    }

    /// Rebuild the in-flight INVITE transactions a call materialised from a
    /// replica names (ADR-0014), each attributed to `call_ref` — see
    /// [`TxnSeed`] for what each seed becomes. Returns how many went in; a
    /// seed whose identity already holds a transaction of its role is skipped
    /// and counted (`txn_seed_skipped`), never displaced, save an occupant
    /// with no final, which takes an Accepted seed's 2xx.
    pub async fn seed(
        &self,
        call_ref: &str,
        seeds: Vec<TxnSeed>,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Seed { call_ref: call_ref.to_string(), seeds, reply }).await
    }

    /// Hand back a datagram this layer already emitted, to be processed
    /// against the transactions now in the map — the ones [`seed`](Self::seed)
    /// just rebuilt. A response matching a client transaction and a CANCEL
    /// matching an active INVITE server transaction run their first-arrival
    /// path; a request matching a server transaction (RFC 3261 §17.2.3) is
    /// absorbed with nothing sent, its arrival already answered (§17.2.1). All
    /// three are [`Reoffer::Matched`] (the consumer drops its copy: whatever
    /// that path emits arrives as a fresh event); anything else is
    /// [`Reoffer::Unmatched`], sent nowhere and emitted nowhere. Under a muted
    /// Call-ID it is [`Reoffer::Muted`] and dropped (ADR-0063 §4).
    pub async fn reoffer(
        &self,
        message: SipMessage,
        src: SocketAddr,
    ) -> Result<Reoffer, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::Reoffer { message: Box::new(message), src, reply }).await
    }

    /// Release every client transaction of `call_ref`, Accepted ones included:
    /// each is orphaned and closes its own obligations, the ACK of each later
    /// 2xx among them, surfacing nothing (ADR-0034). Idempotent.
    pub async fn cancel_txns_for_call(&self, call_ref: &str) -> Result<(), TransactionLayerClosed> {
        self.cancel_txns_for_call_with_origins(call_ref, Vec::new()).await
    }

    /// [`Self::cancel_txns_for_call`], each INVITE client transaction
    /// `origins` names answering an offer its 2xx makes under the origin given
    /// there ([`OrphanOrigin`]).
    pub async fn cancel_txns_for_call_with_origins(
        &self,
        call_ref: &str,
        origins: Vec<OrphanOrigin>,
    ) -> Result<(), TransactionLayerClosed> {
        let call_ref = call_ref.to_string();
        self.roundtrip(|reply| Command::CancelTxnsForCall { call_ref, origins, reply }).await
    }

    /// How many transactions still serve `call_ref`: those on its books (any
    /// role/state), plus the in-dialog server transactions of a Call-ID in
    /// `call_ids` that opened attributed to no call and have sent no final,
    /// until [`attach_server_to_call`](Self::attach_server_to_call) takes
    /// them onto the books. Only the Call-IDs named are read: the
    /// acting-backup self-release (ADR-0014) names those of the copy's legs
    /// and reads it as a defensive re-check — see `Command::ActiveTxnCount`.
    pub async fn active_txn_count_for_call(
        &self,
        call_ref: &str,
        call_ids: &[String],
    ) -> Result<usize, TransactionLayerClosed> {
        let (call_ref, call_ids) = (call_ref.to_string(), call_ids.to_vec());
        self.roundtrip(|reply| Command::ActiveTxnCount { call_ref, call_ids, reply }).await
    }

    /// Whether this layer holds a client transaction on `branch`: the final to
    /// the request sent on it arrives here (RFC 3261 §17.1.3).
    pub async fn holds_client(&self, branch: &str) -> Result<bool, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::HoldsClient { branch: branch.to_string(), reply }).await
    }

    /// The consumer discarded the non-INVITE request whose server transaction
    /// is `key` ([`ServerTxnKey::of`] the request) before answering it: forget
    /// that transaction if it is still `Trying`, so the UAC's retransmission
    /// (RFC 3261 §17.1.2.2) is admitted afresh instead of absorbed unanswered.
    /// Another request on the same branch — another sent-by or method — is
    /// another transaction and is never forgotten, and the request's `call_id`
    /// and `from_tag` must match the transaction's; a transaction that has
    /// sent a response is kept. Never waits and is never refused: the owner
    /// takes it ahead of its commands.
    pub fn forget_unanswered(&self, key: &ServerTxnKey, call_id: &str, from_tag: &str) {
        let forget = Forget {
            key: key.clone(),
            call_id: call_id.to_string(),
            from_tag: from_tag.to_string(),
        };
        // Closed only once the owner is gone, which forgets nothing anyway.
        let _ = self.forget_tx.send(forget);
    }

    /// The consumer resolved the request whose server transaction is `key`
    /// ([`ServerTxnKey::of`] the request) to `call_ref`, its Request-URI naming
    /// no call: the transaction is attributed to `call_ref` while it has sent
    /// no final and is attributed to no call, and from then on it is on the
    /// call's books as one the Request-URI attributed (the self-release count,
    /// the release's forget and answer). One attributed to another call keeps
    /// it and the refusal is counted (`call_attach_refused`); a key naming no
    /// transaction changes nothing. Returns once the command is queued, never
    /// awaiting the owner's turn: every command sent after it is taken after
    /// it.
    pub async fn attach_server_to_call(
        &self,
        key: ServerTxnKey,
        call_ref: &str,
    ) -> Result<(), TransactionLayerClosed> {
        let call_ref = call_ref.to_string();
        let attach = Command::AttachServerToCall { key, call_ref };
        self.cmd_tx.send(attach).await.map_err(|_| TransactionLayerClosed)
    }

    /// The consumer released `call_ref` for good: nothing will answer a request
    /// of it any more. Every non-INVITE server transaction attributed to it
    /// that has sent no final is forgotten, so the UAC's retransmission
    /// (RFC 3261 §17.1.2.2) is admitted afresh — and meets no call — instead
    /// of being absorbed until its Timer F. INVITE transactions are kept.
    /// Returns how many were forgotten.
    pub async fn forget_unanswered_of_call(
        &self,
        call_ref: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::ForgetUnansweredOfCall {
            call_ref: call_ref.to_string(),
            reply,
        })
        .await
    }

    /// The consumer released `call_ref` for good while an INVITE it was
    /// handed still has no final: its 100 Trying stopped the UAC's
    /// retransmissions (RFC 3261 §17.2.1), so nothing would ever answer it.
    /// Every in-dialog INVITE server transaction attributed to the call that
    /// has sent no final is answered `status` `reason` through the
    /// transaction, under the To-tag it names, to where the request came
    /// from. An out-of-dialog INVITE naming the call starts another call and
    /// is left alone. The
    /// transaction then runs Timer G to its ACK, which it absorbs. Returns
    /// how many were answered.
    pub async fn answer_unanswered_invites_of_call(
        &self,
        call_ref: &str,
        status: u16,
        reason: &str,
    ) -> Result<usize, TransactionLayerClosed> {
        self.roundtrip(|reply| Command::AnswerUnansweredInvitesOfCall {
            call_ref: call_ref.to_string(),
            status,
            reason: reason.to_string(),
            reply,
        })
        .await
    }

    /// Release the 100 Trying of the server INVITE transaction `invite`
    /// opened or a seed rebuilt (RFC 3261 §17.2.1): it leaves now if the
    /// transaction has sent nothing, and never after another response.
    /// `true` when the 100 left; `false` too when no transaction holds
    /// `invite`.
    pub async fn release_trying(
        &self,
        invite: &SipRequest,
    ) -> Result<bool, TransactionLayerClosed> {
        let Some(key) = ServerTxnId::of_request(invite).map(ServerTxnId::to_key) else {
            return Ok(false);
        };
        self.roundtrip(|reply| Command::ReleaseTrying { key, reply }).await
    }

    /// Arm the §17.2.1 bound of the server INVITE transaction `invite`
    /// opened, from the instant the layer takes the command: its 100 leaves
    /// [`trying_bound_ms`](crate::TransactionConfig::trying_bound_ms) later
    /// unless a release or a response left first. The bound of an initial
    /// INVITE under
    /// [`consumer_arms_initial_trying`](crate::TransactionConfig::consumer_arms_initial_trying)
    /// runs from here alone. A transaction past `Trying`, or none, arms
    /// nothing. Returns once the command is queued, never awaiting the
    /// owner's turn.
    pub async fn arm_trying(&self, invite: &SipRequest) -> Result<(), TransactionLayerClosed> {
        let Some(key) = ServerTxnId::of_request(invite).map(ServerTxnId::to_key) else {
            return Ok(());
        };
        self.cmd_tx.send(Command::ArmTrying { key }).await.map_err(|_| TransactionLayerClosed)
    }

    /// Release the ACK of the non-2xx final the client INVITE transaction at
    /// `branch` took ([`TransactionConfig::ack_bound_ms`](crate::TransactionConfig)):
    /// it leaves now if it is still held. `true` when it left; `false` when
    /// none is held there.
    pub async fn release_ack(&self, branch: &str) -> Result<bool, TransactionLayerClosed> {
        let branch = branch.to_string();
        self.roundtrip(|reply| Command::ReleaseAck { branch, reply }).await
    }

    /// Release the 200 to a CANCEL and the 487 to the INVITE it cancelled, the
    /// INVITE named by its Call-ID, From-tag and CSeq number as the
    /// `Cancelled` event states them
    /// ([`TransactionConfig::cancel_bound_ms`](crate::TransactionConfig)):
    /// they leave now if still held, an INVITE left open for this consumer
    /// ending as `invite` says
    /// ([`TransactionConfig::consumer_ends_cancelled_reinvite`](crate::TransactionConfig)).
    /// `true` when they left; `false` when none are held for that INVITE.
    pub async fn release_cancel(
        &self,
        call_id: &str,
        from_tag: &str,
        cseq: u32,
        invite: CancelledInvite,
    ) -> Result<bool, TransactionLayerClosed> {
        let (call_id, from_tag) = (call_id.to_string(), from_tag.to_string());
        self.roundtrip(|reply| Command::ReleaseCancel { call_id, from_tag, cseq, invite, reply })
            .await
    }

    /// Ask to be notified (via [`TransactionEvent::CallQuiesced`]) when the last
    /// transaction serving `call_ref` clears — the push signal the B2BUA
    /// acting-backup self-release (ADR-0014) arms when it takes a dialog over.
    /// What serves it is what
    /// [`active_txn_count_for_call`](Self::active_txn_count_for_call) counts
    /// with the same `call_ids`: the watch holds while either kind is left,
    /// and an unattributed transaction's final or leaving re-checks it.
    /// Idempotent; a second watch replaces the first's `call_ids`.
    pub async fn watch_self_release(
        &self,
        call_ref: &str,
        call_ids: &[String],
    ) -> Result<(), TransactionLayerClosed> {
        let (call_ref, call_ids) = (call_ref.to_string(), call_ids.to_vec());
        self.roundtrip(|reply| Command::WatchSelfRelease { call_ref, call_ids, reply }).await
    }
}

/// Returned by every [`TransactionLayer`] method when the owner task is no longer
/// running (the endpoint closed, or [`TransactionLayer::abort_owner`] was called).
/// Lets callers wind down gracefully instead of panicking on a dead owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLayerClosed;

impl std::fmt::Display for TransactionLayerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transaction layer owner task is no longer running")
    }
}

impl std::error::Error for TransactionLayerClosed {}
