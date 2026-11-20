//! The stateless path's answer to a datagram the parser refused (RFC 3261
//! §16.3): the 400 [`crate::parse_refusals::ParseRefusals`] builds, sent as the
//! proxy's own final, whose ACK is recognised and absorbed (`route`): by its
//! To-tag, or for an in-dialog INVITE, whose 400 echoes the dialog's To-tag, by
//! the refusal memo; or nothing. Each outcome is counted.

use std::net::SocketAddr;

use sip_message::SipParseError;
use sip_txn::parse_refusal::ParseRefusal;

use crate::observability::metrics::Face;

use super::super::ProxyCore;
use super::Arrival;

impl ProxyCore {
    /// Answer the datagram from `src` the parser refused with `error`, or drop
    /// it, and count which. The Warning names the advertised address of `face`,
    /// the one the datagram reached.
    pub(in crate::core) async fn refuse_unparsed(
        &self,
        error: &SipParseError,
        src: SocketAddr,
        face: Face,
    ) {
        let agent = || self.advertised_of(face).to_string();
        let refusal = self.parse_refusals.refuse(error, src, agent, self.now_ms());
        match (refusal, error.partial()) {
            (ParseRefusal::Answer(response), Some(partial)) => {
                let arrival = Arrival { src, face };
                self.refusals.remember_parse_refused(partial, self.now_ms());
                self.send_own_final_to(partial.call_id().as_str(), arrival, &response).await;
                self.metrics.record_parse_refusal_answered();
            }
            (ParseRefusal::Capped, _) => self.metrics.record_parse_refusal_capped(),
            (ParseRefusal::Answer(_) | ParseRefusal::Dropped, _) => {
                self.metrics.record_parse_error();
            }
        }
    }
}
