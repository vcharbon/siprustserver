//! The stateless path's answer to a datagram the parser refused (RFC 3261
//! §16.3): the 400 [`crate::refusals::Refusals::refuse_unparsed`] builds, sent
//! to the source as the proxy's own final, so the ACK to it is absorbed here;
//! or nothing. Each outcome is counted.

use std::net::SocketAddr;

use sip_message::SipParseError;
use sip_txn::parse_refusal::ParseRefusal;

use super::super::ProxyCore;
use super::reply::Answered;

impl ProxyCore {
    /// Answer the datagram from `src` the parser refused with `error`, or drop
    /// it, and count which.
    pub(in crate::core) async fn refuse_unparsed(&self, error: &SipParseError, src: SocketAddr) {
        let agent = self.advertised_for_ip(src.ip()).to_string();
        match self.refusals.refuse_unparsed(error, &agent, self.now_ms()) {
            ParseRefusal::Answer(response) => {
                if let Some(partial) = error.partial() {
                    let answered = Answered {
                        method: partial.method(),
                        top_via: partial.via().first(),
                        call_id: partial.call_id().as_str(),
                        from_tag: partial.from().tag(),
                        cseq: partial.cseq().seq(),
                    };
                    self.send_own_final_to(answered, src, &response).await;
                }
                self.metrics.record_parse_refusal_answered();
            }
            ParseRefusal::Capped => self.metrics.record_parse_refusal_capped(),
            ParseRefusal::Dropped => self.metrics.record_parse_error(),
        }
    }
}
