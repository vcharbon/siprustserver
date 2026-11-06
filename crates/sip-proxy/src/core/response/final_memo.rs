//! What a relayed INVITE final leaves in the branch memo (ADR-0009 X7).
//!
//! The INVITE's entry, which served its CANCEL, now serves only the ACK
//! window: it is shortened to Timer H (RFC 3261 §17.2.1). An entry whose
//! final crosses the other instance lives to its cap.
//!
//! A non-2xx final leaves its ACK hop, whether or not the INVITE crossed this
//! instance: the node the final ARRIVED from, which after a failover is the
//! survivor, not the INVITE's target. Only a caller's ACK follows it; a worker's
//! ACK goes where its INVITE went (RFC 3261 §17.1.1.2). This transaction-less proxy (ADR-0022
//! X4) synthesizes no §17.1.1.3 hop ACK: the UAS retransmits the final through
//! this relay until the upstream's own ACK arrives, which the request path
//! relays on this hop. The branch the sender's server transaction is keyed on
//! is §16.11's function of the ACK itself (`crate::branch`). Gating the relay
//! on this memo, not on the INVITE's entry, keeps a takeover worker's 2xx ACK
//! safe when its reset `IdGen` re-mints a branch aliasing the dead primary's
//! INVITE: no non-2xx final was relayed for a confirmed call (see
//! `core/request`). Short TTL: the upstream ACKs within the final's
//! retransmit window, and a re-sent final refreshes it.

use std::net::SocketAddr;

use sip_message::header::Via;
use sip_message::{Method, SipResponse};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{ack_hop_key, invite_txn_key, CancelEntry, Lifetime};

use super::super::ProxyCore;

impl ProxyCore {
    /// Record what the INVITE final `resp`, relayed with `ours` on top of
    /// `next` (the upstream's Via) and received from `src`, leaves for its
    /// ACK.
    pub(super) fn remember_final(
        &self,
        resp: &SipResponse,
        next: &Via,
        ours: &Via,
        src: SocketAddr,
    ) {
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
        self.cancel_lru.shorten(&key);
        if !(300..700).contains(&resp.status()) {
            return;
        }
        self.cancel_lru.remember(
            &ack_hop_key(next, call_id.as_str(), from.tag(), cseq.seq()),
            CancelEntry {
                target: ProxyAddr::from(src),
                branch: ours.branch().unwrap_or_default().to_owned(),
                stickiness: None,
                minted: None,
                via_cookie: None,
            },
            Lifetime::Retransmission,
        );
    }
}
