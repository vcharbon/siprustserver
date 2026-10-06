//! Seeding (ADR-0014 materialisation): rebuilding INVITE transactions this
//! layer never saw the first copy of, and re-offering the datagram that
//! arrived before they existed. The FSMs the seeds then run are `layer::client`
//! and `layer::server`; nothing protocol-shaped lives here.

use std::net::SocketAddr;

use sip_message::{Method, SipMessage, SipRequest};
use sip_net::UdpEndpoint;

use crate::event::{TimeoutKind, TxnKind};
use crate::seed::{Reoffer, TxnSeed};
use crate::timers::ms;

use super::client::{extract_via_custom_params, ViaMarks};
use super::key::ServerTxnKey;
use super::owner::Owner;
use super::txn::{NewTransaction, Timer, TxnId, TxnState};

impl Owner {
    /// Put every seed in the map of its role as a `Proceeding` INVITE
    /// transaction and return how many went in. A seed whose branch already
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
                TxnSeed::ClientInvite { invite, dest } => {
                    self.seed_client_invite(call_ref, invite, dest)
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

    /// A `Proceeding` client INVITE keyed by `invite`'s branch, attributed
    /// from its Via as `do_send_request` attributes the INVITEs it sends
    /// (falling back to `call_ref` when the Via names no `cr`), with the INVITE
    /// bound armed and no retransmit ladder: the peer answered the first copy
    /// with a provisional the dead node consumed, so Timer A is over and Timer
    /// B is not the deadline any more. The bound runs from the seed: the first
    /// copy's send instant is not in the record.
    fn seed_client_invite(&mut self, call_ref: &str, invite: SipRequest, dest: SocketAddr) {
        let branch = invite.top_via().branch().unwrap_or_default().to_string();
        let ViaMarks { call_ref: via_call_ref, leg_id, incarnation_mark } =
            extract_via_custom_params(&invite);
        let bound = self
            .timers
            .insert(Timer::ClientTimeout(branch.clone()), ms(self.invite_initial_timeout_ms));
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
            state: TxnState::Proceeding,
            destination: Some(dest),
        });
        txn.timeout_kind = TimeoutKind::Transaction;
        txn.timeout_key = Some(bound);
    }

    /// Process `message` as an inbound datagram against the transactions now
    /// in the map, building none and emitting nothing for a miss: a response
    /// matching a client transaction and a CANCEL matching an active INVITE
    /// server transaction run their first-arrival path (a non-2xx INVITE final
    /// is ACKed and held for Timer D, a CANCEL draws 200 + 487 and `Cancelled`)
    /// and re-emit with `matched_client_txn = true`; a request a server
    /// transaction matches (§17.2.3) draws that transaction's cached response —
    /// a 100 Trying composed now when a seed holds none yet — the
    /// retransmission it is (RFC 3261 §17.2.1).
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
            SipMessage::Request(req) => self.replay_cached(endpoint, &req, src).await,
        };
        if matched {
            Reoffer::Matched
        } else {
            Reoffer::Unmatched
        }
    }
}
