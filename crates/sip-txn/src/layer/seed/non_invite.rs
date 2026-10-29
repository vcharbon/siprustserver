//! The non-INVITE seeds: a client transaction re-sending its request, a
//! server transaction absorbing copies until its final, and a server
//! transaction Completed on the final the record holds (RFC 3261 §17.1.2,
//! §17.2.2).

use std::net::SocketAddr;

use sip_message::received::response_destination;
use sip_message::{SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use call::TxnKind;

use super::super::key::{ServerTxnId, ServerTxnKey};
use super::super::owner::Owner;
use super::super::txn::{NewTransaction, TxnId, TxnRef, TxnState};
use super::super::via_marks::{extract_via_custom_params, ViaMarks};

impl Owner {
    /// A client non-INVITE keyed by `request`'s branch, attributed from its
    /// Via as `do_send_request` attributes what it sends (falling back to
    /// `call_ref`), `Trying`, sending `request`'s image once now, ahead of
    /// anything sent after the seed, then on its Timer E ladder from now — the
    /// next copy T1 after the seed — and giving up `bound_ms` from now, at most
    /// Timer F. `false` when the request names no branch or a client
    /// transaction holds it.
    pub(super) async fn seed_client_non_invite(
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
    /// until the consumer's final leaves on it (RFC 3261 §17.2.2), to the top
    /// Via's response target (§18.2.2). `false`
    /// when the request names no branch or its identity is occupied — the
    /// occupant is the copy that triggered the materialisation.
    pub(super) fn seed_server_non_invite(
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
            destination: response_destination(request.top_via()),
        });
        true
    }

    /// A server non-INVITE Completed on the final `response` (RFC 3261
    /// §17.2.2), keyed by its top Via and method, held for Timer J from now,
    /// `response` sent once now toward the transaction's destination — the
    /// top Via's response target, `dest` where it names none — and to each
    /// copy of its request: the record may be ahead of the wire, so the first
    /// copy may never have left, and a repeat is absorbed by the client
    /// transaction (§17.1.2.2). An occupant that has sent no final, opened by
    /// the copy that triggered the materialisation, takes `response` and keeps
    /// its own destination. `false` when the response names no branch or its
    /// identity holds a transaction that has sent its final.
    pub(super) async fn seed_answered_non_invite(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        response: SipResponse,
        dest: SocketAddr,
        leg_id: Option<String>,
    ) -> bool {
        let Some(key) = ServerTxnId::of_response(&response).map(ServerTxnId::to_key) else {
            return false;
        };
        let to = match self.server(key.id()) {
            Some(occupant) if !occupant.state.is_active() => return false,
            Some(occupant) => occupant.destination.unwrap_or(dest),
            None => {
                let destination = response_destination(response.top_via()).unwrap_or(dest);
                self.open_txn(NewTransaction {
                    id: TxnId::Server(key.clone()),
                    kind: TxnKind::NonInvite,
                    method: response.cseq().method().clone(),
                    cseq: response.cseq().seq(),
                    call_id: response.call_id().as_str().to_string(),
                    from_tag: response.from().tag().unwrap_or_default().to_string(),
                    original_request: None,
                    call_ref: Some(call_ref.to_string()),
                    leg_id,
                    incarnation_mark: None,
                    state: TxnState::Completed,
                    destination: Some(destination),
                });
                self.hold_seeded_identity(TxnRef::Server(key.id()));
                destination
            }
        };
        if let Some(txn) = self.server_mut(key.id()) {
            txn.state = TxnState::Completed;
            txn.last_response = Some(response.image().clone());
            txn.last_response_status = Some(response.status());
            txn.original_request = None;
        }
        self.arm_final_hold(key.id(), TxnKind::NonInvite, response.status());
        self.send_buffer(endpoint, response.image(), to).await;
        true
    }
}
