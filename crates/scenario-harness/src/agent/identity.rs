//! What an [`Agent`] mints on what it sends: branch, tag, Via and Contact,
//! and the sent-by its Via names ([`Agent::behind_nat`]).

use std::net::SocketAddr;

use sip_message::header::{self, Uri, Via};
use sip_message::SipStr;

use super::Agent;

impl Agent {
    /// This UA behind a port-translating NAT: its requests name `advertised`
    /// as their Via sent-by and ask for `rport` (RFC 3581 §4), while it sends
    /// from and listens on its own socket. A server answers it at the source
    /// its request came from (RFC 3261 §18.2.2, RFC 3581 §4), never at
    /// `advertised`. Its Contact stays its socket, so the requests of its
    /// dialogs reach it.
    pub fn behind_nat(mut self, advertised: SocketAddr) -> Self {
        self.advertised = Some(advertised);
        self
    }

    pub(super) fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    pub(super) fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    pub(super) fn via(&self) -> Via {
        let branch = SipStr::owned(&self.branch());
        match self.advertised {
            Some(at) => Via::udp(SipStr::owned(&at.ip().to_string()), at.port())
                .with_branch(branch)
                .requesting_rport(),
            None => Via::udp(SipStr::owned(&self.addr.ip().to_string()), self.addr.port())
                .with_branch(branch),
        }
    }
    pub(super) fn contact(&self) -> header::Contact {
        header::Contact::from_uri(
            Uri::sip_user(SipStr::owned(&self.name), SipStr::owned(&self.addr.ip().to_string()))
                .with_port(self.addr.port()),
        )
    }
}
