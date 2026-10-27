//! Seeding (ADR-0014 materialisation, ADR-0055 §2): rebuilding transactions
//! this layer never saw the first copy of, and re-offering the datagram that
//! arrived before they existed. The FSMs the seeds then run are `layer::client`
//! and `layer::server`; nothing protocol-shaped lives here.

use std::net::SocketAddr;

use sip_message::{serialize, Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::seed::{Reoffer, TxnSeed};
use crate::timers::ms;
use call::{TxnKind, TxnTimeoutKind};

use super::key::{ServerTxnId, ServerTxnKey};
use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, HeldCancel, NewTransaction, Timer, TxnId, TxnRef, TxnState};
use super::via_marks::{extract_via_custom_params, ViaMarks};

impl Owner {
    /// Put every seed in the map of its role, with the CANCEL a client INVITE
    /// seed carries, and return how many went in. A seed whose
    /// identity already holds a transaction of its role is skipped and counted,
    /// never displaced: the occupant is either the datagram that triggered the
    /// materialisation or a transaction this node built itself, and both
    /// outrank a rebuild. The one exception is an occupant with no final on
    /// an Accepted seed's identity: it takes the seed's 2xx, and that counts
    /// as seeded. Every seed is attributed to `call_ref` — a server seed
    /// included, where the admit path attributes an initial INVITE to no call
    /// — so the self-release count (ADR-0014) holds the copy until the seed
    /// clears. Every seed's lifetime runs from the seed: an active seed's
    /// backstop is an active INVITE's, measured from now, so a taken-over
    /// INVITE lives at most one bound longer than it would have on the node
    /// that sent or admitted it; an Accepted seed is held for Timer L from now.
    pub(super) async fn do_seed(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        seeds: Vec<TxnSeed>,
    ) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mut seeded = 0usize;
        for seed in seeds {
            let went_in = match seed {
                TxnSeed::ClientInvite { invite, dest, provisional, cancel } => {
                    self.seed_client_invite(call_ref, invite, dest, provisional, cancel)
                }
                TxnSeed::ServerInvite { request, to_tag, leg_id } => {
                    self.seed_server_invite(call_ref, request, to_tag, leg_id)
                }
                TxnSeed::ServerAccepted { response, leg_id } => {
                    self.seed_accepted(endpoint, call_ref, response, leg_id).await
                }
                TxnSeed::ClientNonInvite { request, dest, bound_ms } => {
                    self.seed_client_non_invite(endpoint, call_ref, request, dest, bound_ms).await
                }
                TxnSeed::ServerNonInvite { request, leg_id } => {
                    self.seed_server_non_invite(call_ref, request, leg_id)
                }
            };
            if went_in {
                seeded += 1;
            } else {
                self.metrics.txn_seed_skipped.fetch_add(1, Relaxed);
            }
        }
        self.metrics.txn_seeded.fetch_add(seeded as u64, Relaxed);
        seeded
    }

    /// A server INVITE holding `request`, keyed by its top Via, `Proceeding`
    /// with no response cached: a retransmission draws a 100 Trying
    /// (`replay_cached`). `false` when the request names no branch or its
    /// identity is occupied.
    fn seed_server_invite(
        &mut self,
        call_ref: &str,
        request: SipRequest,
        to_tag: Option<String>,
        leg_id: Option<String>,
    ) -> bool {
        let Some(key) = ServerTxnKey::of(&request) else { return false };
        if self.server(key.id()).is_some() {
            return false;
        }
        let head = NewTransaction {
            id: TxnId::Server(key.clone()),
            kind: TxnKind::Invite,
            method: Method::Invite,
            cseq: request.cseq().seq(),
            call_id: request.call_id().as_str().to_string(),
            from_tag: request.from().tag().unwrap_or_default().to_string(),
            original_request: Some(request),
            call_ref: Some(call_ref.to_string()),
            leg_id,
            incarnation_mark: None,
            state: TxnState::Proceeding,
            destination: None,
        };
        self.open_txn_with(head, |txn| txn.uas_to_tag = to_tag.filter(|t| !t.is_empty()));
        self.hold_seeded_identity(TxnRef::Server(key.id()));
        true
    }

    /// A server INVITE Accepted on `response` (RFC 6026 §7.1), keyed by its top
    /// Via. An occupant that has sent no final takes `response` and sends it to
    /// the address its INVITE came from: it answered that copy with a
    /// provisional only. `false` when the response names no branch or its
    /// identity holds a transaction that has sent its final.
    async fn seed_accepted(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        response: SipResponse,
        leg_id: Option<String>,
    ) -> bool {
        let Some(key) = ServerTxnId::of_response(&response).map(ServerTxnId::to_key) else {
            return false;
        };
        if let Some(occupant) = self.server(key.id()) {
            if !occupant.state.is_active() {
                return false;
            }
            let dest = occupant.destination;
            self.accept(key.id(), &response);
            if let Some(dest) = dest {
                self.send_buffer(endpoint, response.image(), dest).await;
            }
            return true;
        }
        self.open_txn(NewTransaction {
            id: TxnId::Server(key.clone()),
            kind: TxnKind::Invite,
            method: Method::Invite,
            cseq: response.cseq().seq(),
            call_id: response.call_id().as_str().to_string(),
            from_tag: response.from().tag().unwrap_or_default().to_string(),
            original_request: None,
            call_ref: Some(call_ref.to_string()),
            leg_id,
            incarnation_mark: None,
            state: TxnState::Accepted,
            destination: None,
        });
        self.hold_seeded_identity(TxnRef::Server(key.id()));
        self.accept(key.id(), &response);
        true
    }

    /// The server INVITE transaction `id` names, Accepted on the 2xx
    /// `response` (RFC 6026 §7.1): the 2xx cached for its INVITE's copies,
    /// its To-tag bound, held for Timer L from now.
    fn accept(&mut self, id: ServerTxnId<'_>, response: &SipResponse) {
        self.record_uas_tag(id, response);
        if let Some(txn) = self.server_mut(id) {
            txn.state = TxnState::Accepted;
            txn.last_response = Some(response.image().clone());
            txn.last_response_status = Some(response.status());
            txn.original_request = None;
        }
        self.hold(TxnRef::Server(id), Hold::TimerL);
    }

    /// Hold the identity of the server seed `id` names in the node's shared
    /// refusals while it lives, as the admit path does for an INVITE it
    /// admits, so no stage ahead of this layer refuses a copy of it.
    fn hold_seeded_identity(&mut self, id: TxnRef<'_>) {
        let Some(refusals) = self.refusals.clone() else { return };
        let Some(txn) = self.txn_mut(id) else { return };
        txn.held = true;
        refusals.hold(txn.branch(), &txn.call_id, &txn.from_tag);
    }

    /// A client INVITE keyed by `invite`'s branch, attributed from its Via as
    /// `do_send_request` attributes the INVITEs it sends (falling back to
    /// `call_ref` when the Via names no `cr`), with the INVITE bound armed and
    /// no retransmit ladder: the node that sent it ran Timer A. It opens
    /// `Proceeding` when the record holds a provisional for it, else `Trying`,
    /// so a CANCEL on it waits for one (RFC 3261 §9.1). A `Trying` seed
    /// re-sends its INVITE once, at the seed, as a client in `Trying`
    /// retransmits (§17.1.1.2): the callee's server transaction answers that
    /// copy with its latest provisional (§17.2.1), so the copy learns a
    /// provisional the record lost, PRACKs a reliable one, and releases a
    /// CANCEL held on it. The bound runs from the seed: the first copy's send
    /// instant is not in the record. `cancel` is held on it
    /// ([`seed_held_cancel`](Self::seed_held_cancel)). `false` when the INVITE
    /// names no branch or a client transaction holds it.
    fn seed_client_invite(
        &mut self,
        call_ref: &str,
        invite: SipRequest,
        dest: SocketAddr,
        provisional: bool,
        cancel: Option<Box<SipRequest>>,
    ) -> bool {
        let Some(branch) = invite.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
        else {
            return false;
        };
        if self.clients.contains_key(&branch) {
            return false;
        }
        let ViaMarks { call_ref: via_call_ref, leg_id, incarnation_mark } =
            extract_via_custom_params(&invite);
        let bound =
            self.arm(Timer::ClientTimeout(branch.clone()), ms(self.invite_initial_timeout_ms));
        let resend = (!provisional)
            .then(|| bytes::Bytes::from(serialize(&SipMessage::Request(invite.clone()))));
        let head = NewTransaction {
            id: TxnId::Client(branch.clone()),
            kind: TxnKind::Invite,
            method: Method::Invite,
            cseq: invite.cseq().seq(),
            call_id: invite.call_id().as_str().to_string(),
            from_tag: invite.from().tag().unwrap_or_default().to_string(),
            original_request: Some(invite),
            call_ref: via_call_ref.or_else(|| Some(call_ref.to_string())),
            leg_id,
            incarnation_mark,
            state: if provisional { TxnState::Proceeding } else { TxnState::Trying },
            destination: Some(dest),
        };
        self.open_txn_with(head, |txn| {
            txn.timeout_kind = TxnTimeoutKind::Transaction;
            txn.timeout_key = Some(bound);
            txn.seed_resend = resend;
        });
        if let Some(cancel) = cancel {
            self.seed_held_cancel(&branch, *cancel, dest);
        }
        self.release_seed_resend(&branch);
        true
    }

    /// A client non-INVITE keyed by `request`'s branch, attributed from its
    /// Via as `do_send_request` attributes what it sends (falling back to
    /// `call_ref`), `Trying`, sending `request`'s image once now, ahead of
    /// anything sent after the seed, then on its Timer E ladder from now — the
    /// next copy T1 after the seed — and giving up `bound_ms` from now, at most
    /// Timer F. `false` when the request names no branch or a client
    /// transaction holds it.
    async fn seed_client_non_invite(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        request: SipRequest,
        dest: SocketAddr,
        bound_ms: u64,
    ) -> bool {
        let Some(branch) = request.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
        else {
            return false;
        };
        if self.clients.contains_key(&branch) {
            return false;
        }
        let ViaMarks { call_ref: via_call_ref, leg_id, incarnation_mark } =
            extract_via_custom_params(&request);
        let buf = request.image().clone();
        let (timer_f, timeout_kind) = self.client_timeout(TxnKind::NonInvite, &request);
        let max_ms = bound_ms.min(timer_f);
        self.open_txn(NewTransaction {
            id: TxnId::Client(branch.clone()),
            kind: TxnKind::NonInvite,
            method: request.method().clone(),
            cseq: request.cseq().seq(),
            call_id: request.call_id().as_str().to_string(),
            from_tag: request.from().tag().unwrap_or_default().to_string(),
            original_request: None,
            call_ref: via_call_ref.or_else(|| Some(call_ref.to_string())),
            leg_id,
            incarnation_mark,
            state: TxnState::Trying,
            destination: Some(dest),
        });
        self.start_client_retransmit(&branch, buf.clone(), dest, max_ms, timeout_kind);
        self.send_buffer(endpoint, &buf, dest).await;
        true
    }

    /// A server non-INVITE holding no response, keyed by `request`'s top Via
    /// and method, `Trying`: a retransmission is absorbed with nothing sent
    /// until the consumer's final leaves on it (RFC 3261 §17.2.2). `false`
    /// when the request names no branch or its identity is occupied — the
    /// occupant is the copy that triggered the materialisation.
    fn seed_server_non_invite(
        &mut self,
        call_ref: &str,
        request: SipRequest,
        leg_id: Option<String>,
    ) -> bool {
        let Some(key) = ServerTxnKey::of(&request) else { return false };
        if self.server(key.id()).is_some() {
            return false;
        }
        self.open_txn(NewTransaction {
            id: TxnId::Server(key.clone()),
            kind: TxnKind::NonInvite,
            method: request.method().clone(),
            cseq: request.cseq().seq(),
            call_id: request.call_id().as_str().to_string(),
            from_tag: request.from().tag().unwrap_or_default().to_string(),
            original_request: None,
            call_ref: Some(call_ref.to_string()),
            leg_id,
            incarnation_mark: None,
            state: TxnState::Trying,
            destination: None,
        });
        true
    }

    /// Re-send, once, the INVITE of the seeded `Trying` transaction at
    /// `branch`: the callee's server transaction answers it with its latest
    /// provisional (RFC 3261 §17.2.1). No ladder: the one fire re-arms
    /// nothing. A transaction past its bound, or whose INVITE was already
    /// re-sent, re-sends nothing.
    pub(super) fn release_seed_resend(&mut self, branch: &str) {
        let Some(buf) = self
            .clients
            .get_mut(branch)
            .filter(|t| !t.lifetime.gave_up())
            .and_then(|t| t.seed_resend.take())
        else {
            return;
        };
        let key = self.arm(Timer::ClientRetransmit(branch.to_string()), ms(0));
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
