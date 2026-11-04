//! RFC 3261 §17.1.1.3 for an INVITE its caller gave up on: once the caller
//! CANCELs and stops waiting (§9.1), its INVITE client transaction still ACKs
//! the non-2xx final the INVITE draws, on the INVITE's branch and path, and
//! re-sends that same ACK each time a final arrives again (§17.1.1.2,
//! "Completed").

use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::{Method, SipRequest, SipResponse};

/// The INVITEs given up on, with the wire destination each went to.
#[derive(Default)]
pub(super) struct AbandonedInvites {
    invites: Mutex<Vec<Abandoned>>,
}

struct Abandoned {
    invite: SipRequest,
    wire_dst: SocketAddr,
    /// The ACK of the first final, once one arrived.
    ack: Option<SipRequest>,
}

impl AbandonedInvites {
    pub(super) fn abandon(&self, invite: SipRequest, wire_dst: SocketAddr) {
        self.invites.lock().unwrap().push(Abandoned { invite, wire_dst, ack: None });
    }

    /// The ACK `resp` draws, and where it goes, when `resp` is a non-2xx final
    /// to an abandoned INVITE: same Call-ID, CSeq number and top-Via branch.
    pub(super) fn ack_for(&self, resp: &SipResponse) -> Option<(SipRequest, SocketAddr)> {
        if resp.status() < 300 || resp.cseq().method() != Method::Invite {
            return None;
        }
        let branch = resp.top_via().branch()?;
        let mut invites = self.invites.lock().unwrap();
        let abandoned = invites.iter_mut().find(|a| {
            a.invite.call_id() == resp.call_id()
                && a.invite.cseq().seq() == resp.cseq().seq()
                && a.invite.top_via().branch() == Some(branch)
        })?;
        if abandoned.ack.is_none() {
            abandoned.ack = generate_ack_for_non_2xx(&abandoned.invite, resp, &[]).ok();
        }
        Some((abandoned.ack.clone()?, abandoned.wire_dst))
    }
}
