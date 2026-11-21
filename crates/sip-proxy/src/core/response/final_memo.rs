//! What a relayed INVITE final leaves in the branch memo (ADR-0009 X7).
//!
//! The INVITE's entry, which served its CANCEL, now serves only the ACK
//! window: it is shortened to Timer H (RFC 3261 §17.2.1). An entry whose
//! final crosses the other instance lives to its cap.
//!
//! A non-2xx final leaves its ACK hop, whether or not the INVITE crossed this
//! instance: the node the final arrived from, which after a failover is the
//! survivor, not the INVITE's target. Only a caller's ACK follows it; a
//! worker's ACK goes where its INVITE went (§17.1.1.2). The proxy keeps no
//! transaction (ADR-0022 X4) and sends no §17.1.1.3 ACK of its own: the UAS
//! retransmits the final through this relay until the upstream's ACK arrives,
//! and the request path relays that ACK on this hop, on the branch §16.11
//! derives from the ACK itself (`crate::branch`). A confirmed call's 2xx ACK
//! finds no hop: no non-2xx final was relayed for it. The hop lives one
//! retransmission TTL, and a re-sent final refreshes it.

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
