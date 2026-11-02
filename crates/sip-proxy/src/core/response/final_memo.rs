//! What a relayed INVITE final leaves in the branch memo (ADR-0009 X7).
//!
//! The INVITE's entry, held only for an INVITE placed off its memo-free route,
//! now serves only the ACK window: it is shortened to Timer H. An entry whose
//! final crosses the other instance lives to its cap.
//!
//! The ACK of a non-2xx final (§17.1.1.3) needs an ACK hop only when its
//! memo-free route misses the node the final came from. A worker's ACK
//! repeats its INVITE's Route and Request-URI, which reach where the INVITE
//! went. Any other ACK goes where the INVITE's entry names, else to the
//! call's rendezvous winner among every worker known, alive or not; the hop
//! is kept when the final came from elsewhere (a takeover after the INVITE's
//! worker died, an address translated on its way here) or from a worker read
//! not alive, where a cookie the ACK rides would not place it. This
//! transaction-less proxy (ADR-0022 X4) synthesizes no hop ACK: the UAS retransmits the final through this relay
//! until the upstream's own ACK arrives, which the request path relays on
//! the hop kept here. A takeover worker's 2xx ACK whose reset `IdGen` re-mints
//! a branch aliasing the dead primary's INVITE stays safe: it rides the
//! dialog's cookie and, as a worker's request, its Route and Request-URI,
//! all read before the INVITE's entry (see `core/request`).

use std::net::SocketAddr;

use sip_message::header::Via;
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{ack_hop_key, invite_txn_key, CancelEntry, Lifetime};
use crate::strategy::{RankPlacement, Ranked};

use super::super::ProxyCore;

impl ProxyCore {
    /// Record what the INVITE final `msg`, relayed with `ours` on top of
    /// `next` (the upstream's Via, sent by `sent_by`) and received from `src`,
    /// leaves for its ACK.
    pub(super) fn remember_final(
        &self,
        msg: &SipMessage,
        next: &Via,
        ours: &Via,
        sent_by: &ProxyAddr,
        src: SocketAddr,
    ) {
        let SipMessage::Response(resp) = msg else { return };
        let cseq = resp.cseq();
        if resp.status() < 200 || *cseq.method() != Method::Invite {
            return;
        }
        // The response echoes the request's From (tag included), and the Via
        // below ours is the upstream's as the INVITE carried it, so this
        // re-builds exactly the key the INVITE was remembered under.
        let call_id = resp.call_id();
        let from = resp.from();
        let key = invite_txn_key(next, call_id.as_str(), from.tag(), cseq.seq());
        let placed = self.cancel_lru.shorten(&key);
        if !(300..700).contains(&resp.status()) {
            return;
        }
        let final_from = ProxyAddr::from(src);
        let hop_key = ack_hop_key(next, call_id.as_str(), from.tag(), cseq.seq());
        if self.ack_reaches(msg, sent_by, placed.as_ref(), &final_from) {
            // A hop kept for an earlier final from elsewhere would misdirect it.
            self.cancel_lru.forget(&hop_key);
            return;
        }
        self.cancel_lru.remember(
            &hop_key,
            CancelEntry {
                target: final_from,
                branch: ours.branch().unwrap_or_default().to_owned(),
                stickiness: None,
                minted: None,
                via_cookie: None,
            },
            Lifetime::Retransmission,
        );
    }

    /// Whether the ACK of the non-2xx final `msg`, routed with no ACK hop,
    /// goes where it must: a worker's (`sent_by` names one) where its INVITE
    /// went; another to `final_from`, an alive sender, through the INVITE's
    /// entry `placed` or the call's rendezvous winner.
    fn ack_reaches(
        &self,
        msg: &SipMessage,
        sent_by: &ProxyAddr,
        placed: Option<&CancelEntry>,
        final_from: &ProxyAddr,
    ) -> bool {
        if self.registry.lookup_by_address(sent_by).is_some() {
            return true;
        }
        // A sender read not alive is not where a cookie the ACK may ride
        // places it now.
        if self.registry.lookup_by_address(final_from).is_some_and(|w| !w.is_alive()) {
            return false;
        }
        match placed {
            Some(entry) => entry.target == *final_from,
            None => match self.strategy.placement_by_rank(msg, Ranked::Known) {
                RankPlacement::Node(owner) => owner == *final_from,
                RankPlacement::NoNode | RankPlacement::Unranked => false,
            },
        }
    }
}
