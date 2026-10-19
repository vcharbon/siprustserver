//! Orphaned client transactions (ADR-0034): a released call's transactions
//! detached from it, and the obligations they then close on their own — the
//! ACK of each 2xx, the BYE of a dialog nothing holds, the re-ACK of repeats.

use bytes::Bytes;
use sip_message::generators::{
    generate_ack_for_2xx_from_invite, generate_bye_for_2xx_from_invite, GenerateError,
};
use sip_message::sdp_answer::reject_offer;
use std::net::SocketAddr;

use sip_message::{BuildHeldSdpOptions, Method, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use call::TxnKind;

use super::owner::Owner;
use super::txn::TxnRef;

impl Owner {
    /// The first 2xx an orphaned INVITE client txn draws: Accepted for Timer
    /// M ([`Owner::accept_2xx`]), then the ACK and BYE the layer owes for it
    /// ([`Owner::release_2xx`]).
    pub(super) async fn accept_orphaned_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        self.accept_2xx(branch, resp);
        self.release_2xx(endpoint, branch, resp).await;
    }

    /// A 2xx on an orphaned Accepted INVITE client txn, the layer standing in
    /// for the UAC core the release removed (RFC 3261 §13.2.2.4, RFC 6026
    /// §7.2): its dialog's ACK, the same one every time. A dialog whose ACK
    /// already left — the TU's, or one built here — draws that ACK again, where
    /// it went, and nothing else. A dialog no ACK left for draws one built from
    /// the INVITE ([`Owner::orphan_ack`]), kept for its repeats, and its BYE
    /// when an initial INVITE opened it and no BYE left on it yet
    /// ([`Owner::bye_unheld_dialog`]). A new dialog past the remembered ones
    /// draws a built ACK and no BYE.
    pub(super) async fn release_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        let Some(t) = self.clients.get_mut(branch) else { return };
        let known = t.dialog(resp.to().tag()).map(|d| (d.ack.clone(), d.byed));
        if let Some((Some((ack, dest)), _)) = known {
            return self.send_buffer(endpoint, &ack, dest).await;
        }
        let Some(invite) = self.accepted_invite(branch) else { return };
        let Some((ack, dest)) = self.orphan_ack(branch, &invite, resp) else { return };
        self.send_buffer(endpoint, &ack, dest).await;
        let Some((_, byed)) = known else { return };
        if let Some(dialog) = self.clients.get_mut(branch).and_then(|t| t.dialog(resp.to().tag())) {
            dialog.ack = Some((ack, dest));
        }
        if !byed {
            self.bye_unheld_dialog(endpoint, branch, &invite, resp).await;
        }
    }

    /// The ACK of `resp` built from `invite`, the INVITE of the client txn at
    /// `branch`, on a fresh branch, answering the offer `resp` carries when one
    /// is owed ([`owed_answer`]), with its next hop ([`next_hop`]); `None`,
    /// logged, where its lines make no ACK or it names no address.
    fn orphan_ack(
        &self,
        branch: &str,
        invite: &SipRequest,
        resp: &SipResponse,
    ) -> Option<(Bytes, SocketAddr)> {
        let t = self.clients.get(branch)?;
        let answer = owed_answer(invite, resp, &t.reliable_offers);
        let ack = generate_ack_for_2xx_from_invite(invite, resp, &self.id_gen.new_branch(), answer)
            .inspect_err(|e| unbuilt(branch, "ACK", e))
            .ok()?;
        let dest = next_hop(&ack).or_else(|| unaddressed(branch, "ACK"))?;
        Some((ack.image().clone(), dest))
    }

    /// End with a BYE (§15.1.1) the dialog `resp`, a 2xx to `invite`, the
    /// orphaned initial INVITE at `branch`, created: numbered above every
    /// request this layer sent on that dialog, sent to its next hop
    /// ([`next_hop`]), on an orphaned non-INVITE transaction of its own. A 2xx
    /// to a re-INVITE belongs to a dialog its call ended.
    async fn bye_unheld_dialog(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        invite: &SipRequest,
        resp: &SipResponse,
    ) {
        let built = self.clients.get(branch).and_then(|t| {
            let orig = Some(invite).filter(|o| o.to().tag().is_none())?;
            let to_tag = resp.to().tag();
            let sent =
                t.dialogs.iter().find(|d| d.to_tag.as_deref() == to_tag).map_or(0, |d| d.cseq);
            let cseq = orig.cseq().seq().max(sent) + 1;
            let bye = generate_bye_for_2xx_from_invite(orig, resp, cseq, &self.id_gen.new_branch())
                .inspect_err(|e| unbuilt(branch, "BYE", e))
                .ok()?;
            let dest = next_hop(&bye).or_else(|| unaddressed(branch, "BYE"))?;
            Some((bye, dest))
        });
        let Some((bye, dest)) = built else { return };
        let bye_branch = bye.top_via().branch().unwrap_or_default().to_string();
        Box::pin(self.do_send_request(endpoint, bye, dest, TxnKind::NonInvite)).await;
        if let Some(t) = self.clients.get_mut(bye_branch.as_str()) {
            t.orphaned = true;
            self.detach_from_call(TxnRef::Client(&bye_branch));
            self.metrics.orphaned_transactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Raise, on every INVITE client txn of `call_ref` that `req`, a request
    /// inside a dialog, shares its Call-ID and From-tag with, the CSeq of the
    /// dialog `req`'s To-tag names, so a BYE the layer later sends on that
    /// dialog numbers above it (§12.2.1.1); a BYE marks the dialog ended, so
    /// the layer sends it none.
    pub(super) fn note_dialog_cseq(&mut self, call_ref: Option<&str>, req: &SipRequest) {
        let Some(to_tag) = req.to().tag() else { return };
        let Some(txns) = call_ref.and_then(|cr| self.txn_index.get(cr)) else { return };
        let (call_id, from_tag) = (req.call_id().as_str(), req.from().tag().unwrap_or_default());
        let seq = req.cseq().seq();
        for branch in txns.every_client() {
            let Some(t) = self.clients.get_mut(branch.as_str()).filter(|t| {
                t.kind == TxnKind::Invite && t.call_id == call_id && t.from_tag == from_tag
            }) else {
                continue;
            };
            if let Some(dialog) = t.dialog(Some(to_tag)) {
                dialog.cseq = dialog.cseq.max(seq);
                dialog.byed |= *req.method() == Method::Bye;
            }
        }
    }

    /// Release every client transaction of `call_ref` from the call (ADR-0034),
    /// Accepted ones included. Each is orphaned, never deleted: it keeps its
    /// timers and closes its own obligations — the ACK a final still draws
    /// (RFC 3261 §17.1.1.3, §13.2.2.4), the Timer D / Timer M re-ACK hold,
    /// Timer E to its final or Timer F — and is purged by its own timer.
    /// Server transactions keep their §17.2 holds.
    pub(super) fn do_cancel_txns_for_call(&mut self, call_ref: &str) {
        // O(k-branches) over just this call's live branches (the lockstep index),
        // not an O(total_txns) scan of the whole map.
        let branches: Vec<String> = match self.txn_index.get(call_ref) {
            Some(txns) => txns.every_client().cloned().collect(),
            None => return,
        };
        for branch in branches {
            // A held CANCEL stays parked on the orphan: only the branch's
            // first provisional sends it (§9.1).
            match self.clients.get_mut(branch.as_str()) {
                Some(t) => t.orphaned = true,
                None => continue,
            }
            self.detach_from_call(TxnRef::Client(&branch));
            self.metrics
                .txn_orphaned_on_call_evict
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics.orphaned_transactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Where an in-dialog `req` goes (RFC 3261 §8.1.2, §12.2.1.1): its first
/// `Route` when that one is loose, else its Request-URI — the remote target,
/// or a strict first hop — when its host is an address.
fn next_hop(req: &SipRequest) -> Option<SocketAddr> {
    let routes = req.route_set().ok()?;
    let uri = match routes.first() {
        Some(route) if route.uri().is_loose_route() => route.uri(),
        _ => req.request_uri(),
    };
    let (host, port) = uri.host_port();
    sip_net::ip_socket_addr(host, port)
}

/// Log that the `what` an orphaned transaction at `branch` owes names no
/// address to send it to: the layer resolves no host name.
fn unaddressed<T>(branch: &str, what: &str) -> Option<T> {
    tracing::warn!(%branch, %what, "orphan obligation not sent: its next hop names no address");
    None
}

/// The answer the ACK of `resp`, a 2xx to `invite`, owes (RFC 3264 §4):
/// `resp` carries an offer when `invite` carried none and no reliable
/// provisional of its dialog did (`reliable_offers`, RFC 3262 §5). Every
/// offered stream is rejected (RFC 3264 §6: port 0): the dialog is released,
/// never used.
fn owed_answer(
    invite: &SipRequest,
    resp: &SipResponse,
    reliable_offers: &[String],
) -> Option<Vec<u8>> {
    let offered_reliably =
        resp.to().tag().is_some_and(|tag| reliable_offers.iter().any(|t| t == tag));
    if invite.sdp().is_some() || offered_reliably {
        return None;
    }
    let options = BuildHeldSdpOptions {
        local_ip: invite.top_via().host().to_string(),
        now_ms: sip_clock::raw_system_wall_ms(),
    };
    reject_offer(resp.sdp()?, &options)
}

/// Log that the `what` an orphaned transaction at `branch` owes was not sent:
/// the lines it echoes make no message.
fn unbuilt(branch: &str, what: &str, e: &GenerateError) {
    tracing::warn!(%branch, %what, error = %e, "orphan obligation not sent: its lines make none");
}
