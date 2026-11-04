//! [`ParseRefusals`]: the proxy's answers to datagrams the parser refused, one
//! instance shared by every recv shard. A request whose refusal carries a
//! partial draws a stateless 400 (RFC 3261 §16.3) under a To-tag derived from
//! the request, so every copy draws the same tag (§8.2.6.2), never
//! remembered; the process's one cap ([`sip_txn::parse_refusal`]) bounds
//! those 400s.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::SipParseError;
use sip_txn::parse_refusal::{refuse, ParseRefusal, ParseRefusalCap};
use sip_txn::{IdGen, StatelessTagger};

/// The process's cap on the 400s answering refused requests, and the tagger
/// of their To-tags.
///
/// Concurrency: one mutex guards the cap, held only to take a token: a
/// datagram with no partial never takes it, and the 400 is built outside it.
pub struct ParseRefusals {
    cap: Mutex<ParseRefusalCap>,
    tagger: StatelessTagger,
}

impl ParseRefusals {
    /// A full cap; the To-tag secret is drawn from `id_gen` once, here: this
    /// instance's own, for a proxy with no shared key (tests, development).
    pub fn new(id_gen: &IdGen) -> Self {
        Self::keyed(StatelessTagger::from_id_gen(id_gen))
    }

    /// A full cap; `tagger` derives the To-tags (a deployment's shared key,
    /// `crate::security::own_final_tag`).
    pub fn keyed(tagger: StatelessTagger) -> Self {
        Self { cap: Mutex::new(ParseRefusalCap::new(Duration::ZERO)), tagger }
    }

    /// Whether `ack` acknowledges a 400 this proxy answered an INVITE with
    /// (RFC 3261 §17.1.1.3).
    pub fn acknowledges_own_final(&self, ack: &sip_message::SipRequest) -> bool {
        self.tagger.acknowledges_own_final(ack)
    }

    /// The fate at `now_ms` of the datagram from `src` the parser refused
    /// with `error`; its 400 names `agent()` in the Warning.
    pub fn refuse(
        &self,
        error: &SipParseError,
        src: SocketAddr,
        agent: impl FnOnce() -> String,
        now_ms: u64,
    ) -> ParseRefusal {
        let take = || {
            crate::poison::locked(&self.cap, crate::poison::PARSE_REFUSAL_CAP)
                .try_take(Duration::from_millis(now_ms))
        };
        refuse(error, src, &self.tagger, agent, take)
    }
}
