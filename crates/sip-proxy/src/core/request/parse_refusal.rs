//! The stateless path's answer to a datagram the parser refused (RFC 3261
//! §16.3): the 400 [`crate::parse_refusals::ParseRefusals`] builds, sent to
//! the source as the proxy's own final, whose ACK is recognised by its To-tag
//! and absorbed (`route`); or nothing. Each outcome is counted.

use std::net::SocketAddr;

use sip_message::SipParseError;
use sip_txn::parse_refusal::ParseRefusal;

use super::super::ProxyCore;

impl ProxyCore {
    /// Answer the datagram from `src` the parser refused with `error`, or drop
    /// it, and count which. The Warning names the advertised address of the
    /// face `src` reached.
    pub(in crate::core) async fn refuse_unparsed(&self, error: &SipParseError, src: SocketAddr) {
        let agent = || self.advertised_for_ip(src.ip()).to_string();
        let refusal = self.parse_refusals.refuse(error, src, agent, self.now_ms());
        match (refusal, error.partial()) {
            (ParseRefusal::Answer(response), Some(partial)) => {
                self.send_own_final_to(partial.call_id().as_str(), src, &response).await;
                self.metrics.record_parse_refusal_answered();
            }
            (ParseRefusal::Capped, _) => self.metrics.record_parse_refusal_capped(),
            (ParseRefusal::Answer(_) | ParseRefusal::Dropped, _) => {
                self.metrics.record_parse_error();
            }
        }
    }
}
