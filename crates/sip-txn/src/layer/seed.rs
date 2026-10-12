//! Seeding (ADR-0014 materialisation): rebuilding INVITE transactions this
//! layer never saw the first copy of, and re-offering the datagram that
//! arrived before they existed. The FSMs the seeds then run are `layer::client`
//! and `layer::server`; nothing protocol-shaped lives here.

use std::net::SocketAddr;

use sip_message::{serialize, Method, SipMessage, SipRequest};
use sip_net::UdpEndpoint;

use crate::event::{TimeoutKind, TxnKind};
use crate::seed::{Reoffer, TxnSeed};
use crate::timers::ms;

use super::client::{extract_via_custom_params, ViaMarks};
use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{CancelWire, HeldCancel, NewTransaction, Timer, TxnId, TxnState};

impl Owner {
    /// Put every seed in the map of its role as an INVITE transaction, with the
    /// CANCEL a client seed carries, and return how many went in. A seed whose branch already
    /// holds a transaction of its role is skipped and counted, never displaced: the occupant is
    /// either the datagram that triggered the materialisation or a transaction
    /// this node built itself, and both outrank a rebuild. Every seed is
    /// attributed to `call_ref` — a server seed included, where the admit path
    /// attributes an initial INVITE to no call — so the self-release count
    /// (ADR-0014) holds the copy until the seed clears. Every seed's lifetime
    /// runs from the seed: its backstop is an active INVITE's, measured from
    /// now, so a taken-over INVITE lives at most one bound longer than it
    /// would have on the node that sent or admitted it.
    pub(super) fn do_seed(&mut self, call_ref: &str, seeds: Vec<TxnSeed>) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mut seeded = 0usize;
        for seed in seeds {
            let occupied = match &seed {
                TxnSeed::ClientInvite { invite, .. } => invite
                    .top_via()
                    .branch()
                    .filter(|b| !b.is_empty())
                    .is_none_or(|b| self.clients.contains_key(b)),
                TxnSeed::ServerInvite { branch, sent_by, .. } => {
                    branch.is_empty()
                        || self.servers.contains_key(&ServerTxnKey::new(
                            branch.clone(),
                            sent_by.clone(),
                            &Method::Invite,
                        ))
                }
            };
            if occupied {
                self.metrics.txn_seed_skipped.fetch_add(1, Relaxed);
                continue;
            }
            match seed {
                TxnSeed::ClientInvite { invite, dest, provisional, cancel } => {
                    let branch = invite.top_via().branch().unwrap_or_default().to_string();
                    self.seed_client_invite(call_ref, invite, dest, provisional);
                    if let Some(cancel) = cancel {
                        self.seed_held_cancel(&branch, *cancel, dest);
                    }
                }
                TxnSeed::ServerInvite {
                    branch,
                    sent_by,
                    call_id,
                    from_tag,
                    to_tag,
                    leg_id,
                    original_request,
                } => {
                    let refusals = self.refusals.clone();
                    let txn = self.open_txn(NewTransaction {
                        id: TxnId::Server(ServerTxnKey::new(branch, sent_by, &Method::Invite)),
                        kind: TxnKind::Invite,
                        method: Method::Invite,
                        call_id,
                        from_tag,
                        original_request,
                        call_ref: Some(call_ref.to_string()),
                        leg_id,
                        incarnation_mark: None,
                        state: TxnState::Proceeding,
                        destination: None,
                    });
                    txn.uas_to_tag = to_tag.filter(|t| !t.is_empty());
                    if let Some(refusals) = refusals {
                        txn.held = true;
                        refusals.hold(txn.branch(), &txn.call_id, &txn.from_tag);
                    }
                }
            }
            seeded += 1;
        }
        self.metrics.txn_seeded.fetch_add(seeded as u64, Relaxed);
        seeded
    }

    /// A client INVITE keyed by `invite`'s branch, attributed from its Via as
    /// `do_send_request` attributes the INVITEs it sends (falling back to
    /// `call_ref` when the Via names no `cr`), with the INVITE bound armed and
    /// no retransmit ladder: the node that sent it ran Timer A. It opens
    /// `Proceeding` when the record holds a provisional for it, else `Trying`,
    /// so a CANCEL on it waits for one (RFC 3261 §9.1); a `Trying` seed keeps
    /// its INVITE to re-send once when a CANCEL is held on it. The bound runs
    /// from the seed: the first copy's send instant is not in the record.
    fn seed_client_invite(
        &mut self,
        call_ref: &str,
        invite: SipRequest,
        dest: SocketAddr,
        provisional: bool,
    ) {
        let branch = invite.top_via().branch().unwrap_or_default().to_string();
        let ViaMarks { call_ref: via_call_ref, leg_id, incarnation_mark } =
            extract_via_custom_params(&invite);
        let bound = self
            .timers
            .insert(Timer::ClientTimeout(branch.clone()), ms(self.invite_initial_timeout_ms));
        let resend = (!provisional)
            .then(|| bytes::Bytes::from(serialize(&SipMessage::Request(invite.clone()))));
        let txn = self.open_txn(NewTransaction {
            id: TxnId::Client(branch.clone()),
            kind: TxnKind::Invite,
            method: Method::Invite,
            call_id: invite.call_id().as_str().to_string(),
            from_tag: invite.from().tag().unwrap_or_default().to_string(),
            original_request: Some(invite),
            call_ref: via_call_ref.or_else(|| Some(call_ref.to_string())),
            leg_id,
            incarnation_mark,
            state: if provisional { TxnState::Proceeding } else { TxnState::Trying },
            destination: Some(dest),
        });
        txn.timeout_kind = TimeoutKind::Transaction;
        txn.timeout_key = Some(bound);
        txn.seed_resend = resend;
    }

    /// Re-send, once, the INVITE of the seeded `Trying` transaction at
    /// `branch` that a CANCEL is now held on: the callee's server transaction
    /// answers it with its latest provisional (RFC 3261 §17.2.1), which
    /// releases the CANCEL. No ladder: the one fire re-arms nothing. A
    /// transaction past its bound re-sends nothing.
    pub(super) fn release_seed_resend(&mut self, branch: &str) {
        let Some(buf) = self
            .clients
            .get_mut(branch)
            .filter(|t| !t.lifetime.gave_up())
            .and_then(|t| t.seed_resend.take())
        else {
            return;
        };
        let key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), ms(0));
        if let Some(txn) = self.clients.get_mut(branch) {
            txn.retransmit_buf = Some(buf);
            txn.retransmit_key = Some(key);
        }
    }

    /// Hold the CANCEL the record says was sent for the seeded client INVITE
    /// at `branch` and still awaits its first provisional (RFC 3261 §9.1).
    fn seed_held_cancel(&mut self, branch: &str, cancel: SipRequest, dest: SocketAddr) {
        let buf = bytes::Bytes::from(serialize(&SipMessage::Request(cancel)));
        let Some(txn) = self.clients.get_mut(branch) else { return };
        txn.held_cancel = Some(HeldCancel::new(buf, dest, CancelWire::Held));
        self.metrics.cancels_held.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.release_seed_resend(branch);
    }

    /// Process `message` as an inbound datagram against the transactions now
    /// in the map, building none and emitting nothing for a miss: a response
    /// matching a client transaction and a CANCEL matching an active INVITE
    /// server transaction run their first-arrival path (a non-2xx INVITE final
    /// is ACKed and held for Timer D, a CANCEL draws 200 + 487 and `Cancelled`)
    /// and re-emit with `matched_client_txn = true`; a request a server
    /// transaction matches (§17.2.3) is absorbed with nothing sent: the layer
    /// answered that datagram when it emitted it, and §17.2.1 replays a
    /// response only to a retransmission received from the wire.
    pub(super) async fn do_reoffer(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        message: SipMessage,
        src: SocketAddr,
    ) -> Reoffer {
        let matched = match message {
            SipMessage::Response(resp) => self.inbound_response(endpoint, resp, src, true).await,
            SipMessage::Request(req) if req.method() == Method::Cancel => {
                self.handle_cancel(endpoint, req, src, true).await
            }
            SipMessage::Request(req) => {
                ServerTxnId::of_request(&req).is_some_and(|id| self.server(id).is_some())
            }
        };
        if matched {
            Reoffer::Matched
        } else {
            Reoffer::Unmatched
        }
    }
}
