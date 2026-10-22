//! The owner's answer to a datagram the parser refused: the stateless 400 of
//! [`crate::parse_refusal`] sent to the datagram's source, opening no
//! transaction, so every copy of the request is answered again and the ACK to
//! the 400 matches nothing here; or nothing. Each outcome is counted.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;

use sip_message::SipParseError;
use sip_net::UdpEndpoint;
use tokio::time::Instant;

use crate::parse_refusal::{ParseRefusal, ParseRefusalCap};
use crate::rng::IdGen;
use crate::StatelessTagger;

use super::owner::Owner;

/// The owner's cap on its 400s, on a timeline starting at `origin`, and the
/// tagger deriving each 400's To-tag from the refused request, so every copy
/// draws the same tag (RFC 3261 §8.2.6.2).
pub(super) struct ParseRefusals {
    cap: ParseRefusalCap,
    origin: Instant,
    tagger: StatelessTagger,
}

impl ParseRefusals {
    pub(super) fn new(id_gen: &IdGen) -> Self {
        Self {
            cap: ParseRefusalCap::new(std::time::Duration::ZERO),
            origin: Instant::now(),
            tagger: StatelessTagger::from_id_gen(id_gen),
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
        let agent = endpoint.egress_local_addr(src).to_string();
        let refusals = &mut self.parse_refusals;
        let now = refusals.origin.elapsed();
        let tagger = &refusals.tagger;
        match refusals.cap.judge(error, &agent, |p| tagger.for_partial(p).0, now) {
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
}
