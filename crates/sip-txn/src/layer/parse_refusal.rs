//! The owner's answer to a datagram the parser refused: the stateless 400 of
//! [`crate::parse_refusal`] sent to the datagram's source, opening no
//! transaction, so every copy of the request is answered again; or nothing.
//! Each outcome is counted. The ACK to such a 400 ends here.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;

use sip_message::{SipParseError, SipRequest};
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use crate::parse_refusal::{refuse, ParseRefusal, ParseRefusalCap};
use crate::rng::IdGen;
use crate::StatelessTagger;

use super::owner::Owner;

/// The owner's cap on its 400s, on a timeline starting at `origin`; the
/// tagger deriving each 400's To-tag from the refused request, so every copy
/// draws the same tag (RFC 3261 §8.2.6.2); and the `hostport` the 400's
/// Warning names, the address it leaves from when `None`.
pub(super) struct ParseRefusals {
    cap: ParseRefusalCap,
    origin: Instant,
    tagger: StatelessTagger,
    advertised: Option<String>,
}

impl ParseRefusals {
    pub(super) fn new(id_gen: &IdGen, advertised: Option<String>) -> Self {
        Self {
            cap: ParseRefusalCap::new(std::time::Duration::ZERO),
            origin: Instant::now(),
            tagger: StatelessTagger::from_id_gen(id_gen),
            advertised,
        }
    }
}

impl Owner {
    /// Answer the datagram from `src` the parser refused with `error`, or drop
    /// it, and count which.
    pub(super) async fn refuse_unparsed(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        error: &SipParseError,
        src: SocketAddr,
    ) {
        let refusals = &mut self.parse_refusals;
        let now = refusals.origin.elapsed();
        let advertised = &refusals.advertised;
        let agent = || match advertised {
            Some(advertised) => advertised.clone(),
            None => endpoint.egress_local_addr(src).to_string(),
        };
        let cap = &mut refusals.cap;
        match refuse(error, src, &refusals.tagger, agent, || cap.try_take(now)) {
            ParseRefusal::Answer(response) => {
                self.send_buffer(endpoint, response.image(), src).await;
                self.metrics.parse_refusals_answered.fetch_add(1, Relaxed);
            }
            ParseRefusal::Capped => {
                self.metrics.parse_refusals_capped.fetch_add(1, Relaxed);
            }
            ParseRefusal::Dropped => {
                self.metrics.parse_errors.fetch_add(1, Relaxed);
            }
        }
    }

    /// Whether the ACK `ack` acknowledges a 400 this owner answered a refused
    /// INVITE with (§17.1.1.3).
    pub(super) fn acknowledges_parse_refusal(&self, ack: &SipRequest) -> bool {
        self.parse_refusals.tagger.acknowledges_own_final(ack)
    }
}
