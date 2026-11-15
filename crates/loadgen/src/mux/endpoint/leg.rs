//! One call leg's endpoint of the mux: its inbox, its sends through the
//! shared socket under the call's loss model, and its release on `Drop`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use sip_message::preparse::is_invite_request_buffer;
use sip_net::queue::PacketQueue;
use sip_net::{SendError, SendNote, SendTap, UdpEndpoint, UdpEndpointCounters, UdpPacket};
use tokio::time::Instant;

use crate::mux::datagram::Datagram;
use crate::mux::late_answer::SentAnswers;
use crate::mux::late_final::SentInvites;
use crate::mux::loss::{DropDir, DropModel};
use crate::mux::retransmit::CallTxns;
use crate::mux::taps::Taps;
use crate::mux::{CallGate, Delivery, Key, MuxSocket, Role, RELEASE_HOLD};

/// One call leg's endpoint: an inbox fed by the shared dispatcher + the shared
/// send socket. Deregisters on `Drop`.
pub(super) struct MuxEndpoint {
    pub(super) local: SocketAddr,
    pub(super) role: Role,
    /// The From key a caller's initial INVITE must yield (from-user
    /// correlation only): the call's key, registered by its callee legs.
    pub(super) caller_key: Option<String>,
    pub(super) mux: Arc<MuxSocket>,
    pub(super) queue: Arc<PacketQueue>,
    pub(super) keyset: Arc<Mutex<Vec<Key>>>,
    /// The call's caller state; a caller opens it with its first INVITE.
    pub(super) gate: Arc<CallGate>,
    /// This caller endpoint has sent its first INVITE.
    pub(super) invite_sent: AtomicBool,
    pub(super) queue_max: usize,
    /// Simulated per-call packet loss (disabled by default), shared with this
    /// call's [`CallTxns`] so retransmits are lossy too. Outbound loss is applied
    /// here in `send_to`; inbound loss is applied in the demux route path (above
    /// the retransmit engine, so a lost inbound datagram is truly gone and the
    /// peer's retransmit re-delivers it fresh).
    pub(super) drop: Arc<DropModel>,
    /// Per-call SIP retransmit engine (present only when `--auto-retransmit` is on
    /// for this call). Records outbound requests/answers and drives their timers.
    pub(super) txns: Option<Arc<CallTxns>>,
    /// This endpoint's taps (present iff the call has an
    /// [`RttSink`](crate::mux::RttSink) or a [`WireTap`](crate::mux::WireTap)),
    /// shared with the inbound `route` path.
    pub(super) taps: Option<Arc<Taps>>,
    /// A caller's INVITEs still open to a non-2xx final; handed to the
    /// registry on `Drop` so a late final is still ACKed.
    pub(super) invites: Option<Arc<SentInvites>>,
    /// The finals this endpoint's agent sent; handed to the registry on
    /// `Drop` so a retransmitted request still draws its answer.
    pub(super) answers: Arc<SentAnswers>,
    /// The recording's send tap (a recorded call only), told of each send
    /// the loss model drops.
    pub(super) sendtap: OnceLock<SendTap>,
}

impl MuxEndpoint {
    /// A caller registers the Call-ID of each dialog it opens, up to and
    /// including its first INVITE, so responses and in-dialog requests demux
    /// back to it. Under from-user correlation that first INVITE must carry
    /// the call's key as its From user: a mismatch would return legs to
    /// another call (or none), so it is refused and counted, never sent.
    /// This is local bookkeeping, done even when the datagram is then
    /// dropped on the wire (a UAC that loses its INVITE still owns the
    /// dialog).
    fn register_dialog(&self, d: &Datagram<'_>) -> Result<(), SendError> {
        if self.role != Role::Caller || self.invite_sent.load(Ordering::Relaxed) {
            return Ok(());
        }
        let invite = is_invite_request_buffer(d.raw());
        if invite {
            if let Some(key) = &self.caller_key {
                let from = self.mux.correlation.token(d.raw());
                if from.as_deref() != Some(key.as_str()) {
                    self.mux.stats.caller_key_mismatch.fetch_add(1, Ordering::Relaxed);
                    return Err(SendError::stated(format!(
                        "caller INVITE From key {from:?} is not the call key {key:?}"
                    )));
                }
            }
        }
        if let Some(cid) = d.id().call_id {
            // Lock order: registry, then keyset (as the demux and `Drop`).
            let mut g = self.mux.reg.lock().unwrap();
            let mut keys = self.keyset.lock().unwrap();
            if !keys.iter().any(|k| matches!(k, Key::CallId(c) if **c == *cid)) {
                let cid: Arc<str> = cid.into();
                g.by_call_id.insert(
                    cid.clone(),
                    Delivery {
                        caller: true,
                        queue: self.queue.clone(),
                        drop: self.drop.clone(),
                        txns: self.txns.clone(),
                        taps: self.taps.clone(),
                        invites: self.invites.clone(),
                        answers: self.answers.clone(),
                    },
                );
                keys.push(Key::CallId(cid));
            }
        }
        if invite {
            self.invite_sent.store(true, Ordering::Relaxed);
            // Release: the Call-ID registered above is visible to the
            // dispatcher that Acquire-loads the gate open.
            self.gate.invite_sent.store(true, Ordering::Release);
        }
        Ok(())
    }
}

#[async_trait]
impl UdpEndpoint for MuxEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        let dropped = {
            let d = Datagram::sniff(buf);
            self.register_dialog(&d)?;
            // Record the outbound message in the retransmit engine BEFORE the
            // loss check: a dropped request is exactly what the resender must
            // recover, and the resender re-applies the same loss model on every
            // retry.
            if let Some(txns) = &self.txns {
                txns.on_outbound(&d, dst);
            }
            if let Some(invites) = &self.invites {
                invites.on_outbound(&d, dst);
            }
            self.answers.on_outbound(&d, self.taps.as_deref());
            // A round trip runs from the first transmission attempt, lost or not.
            if let Some(taps) = &self.taps {
                taps.on_outbound(&d, Instant::now());
            }
            // Simulated loss: report success (the txn believes it sent) but never
            // put the datagram on the wire — the SUT never sees it, so only
            // auto-retransmit recovers the call. The targeted OUTBOUND drop fires
            // here too (a deterministic single-shot the engine's Timer-E resend
            // then heals).
            self.drop.drops() || self.drop.targeted_hit(d.id(), DropDir::Outbound)
        };
        if dropped {
            self.mux.stats.dropped_out.fetch_add(1, Ordering::Relaxed);
            if let Some(tap) = self.sendtap.get() {
                tap(buf, dst, SendNote::LossModel);
            }
            return Ok(());
        }
        self.mux.endpoint.as_ref().send_to(buf, dst).await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.queue.take().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.queue.poll()
    }
    fn local_addr(&self) -> SocketAddr {
        self.local
    }
    fn queue_depth(&self) -> usize {
        self.queue.depth()
    }
    fn queue_max(&self) -> usize {
        self.queue_max
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }

    fn install_recv_tap(&self, tap: sip_net::RecvTap) -> bool {
        self.queue.install_tap(tap);
        true
    }

    /// The tap hears this endpoint's sends the loss model drops and, with
    /// `--auto-retransmit`, the engine's re-emissions: proactive (Timer A/E/G)
    /// and reactive (re-ACK / re-answer), the engine being shared with the
    /// inbound `route` path. The first tap installed is kept.
    fn install_send_tap(&self, tap: SendTap) -> bool {
        if let Some(txns) = &self.txns {
            txns.set_sendtap(tap.clone());
        }
        let _ = self.sendtap.set(tap);
        true
    }
}

impl Drop for MuxEndpoint {
    fn drop(&mut self) {
        // Lock order: registry, then keyset (as the demux, which promotes a
        // leg's Call-ID into its keyset under the registry lock).
        let mut g = self.mux.reg.lock().unwrap();
        let keys: Vec<Key> = self.keyset.lock().unwrap().drain(..).collect();
        let held = |c: &str| (self.invites.as_ref().and_then(|i| i.take(c)), self.answers.take(c));
        g.release_keys(keys, held, Instant::now() + RELEASE_HOLD, &self.mux.stats);
        drop(g);
        self.queue.close();
        // Stop this call's resender tasks now (call ended) rather than letting them
        // run to their transaction timeout — bounded task lifetime under load.
        if let Some(txns) = &self.txns {
            txns.shutdown();
        }
    }
}
