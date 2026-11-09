//! The bound on a party's new request (ADR-0001), met at the hop it first
//! reaches and answered there: the external face of a dual-face proxy, or, on one
//! face, a source that is no registry worker. Anything a worker or a hop sent
//! reads with room.

use std::net::SocketAddr;

use sip_message::{SipParseError, SipRequest};

use crate::observability::metrics::Face;

use super::super::ProxyCore;

impl ProxyCore {
    /// The refusal `req`, received from `src` on `face`, draws as a party's new
    /// request past the bound, with the partial that answers it.
    pub(super) fn first_ingress_refusal(
        &self,
        req: &SipRequest,
        src: SocketAddr,
        face: Face,
    ) -> Option<SipParseError> {
        let refusal = self.parser.first_ingress_refusal(req)?;
        let from_a_party = match &self.external {
            Some(_) => face == Face::External,
            None => !self.sent_by_a_worker(req, src),
        };
        from_a_party.then_some(refusal)
    }
}
