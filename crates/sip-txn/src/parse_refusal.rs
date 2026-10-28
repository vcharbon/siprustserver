//! What an ingress does with a datagram the parser refused: a request whose
//! refusal carries a partial is answered with a stateless `400 Invalid
//! Request` (RFC 3261 §8.2.6, §16.3), anything else is dropped unanswered.
//! One token bucket per ingress caps those 400s, so a flood of malformed
//! requests (whose source address may be forged) draws a bounded rate of
//! answers.

use std::net::SocketAddr;
use std::time::Duration;

use load_shed::TokenBucket;
use sip_message::generators::generate_invalid_request;
use sip_message::{SipParseError, SipResponse};

use crate::StatelessTagger;

/// The 400s an ingress sends back to back before the rate binds.
pub const PARSE_REFUSAL_BURST: u32 = 100;

/// The 400s per second an ingress sends once its burst is spent.
pub const PARSE_REFUSAL_PER_SEC: u32 = 100;

/// The fate of a datagram the parser refused.
#[derive(Debug)]
pub enum ParseRefusal {
    /// The 400 to send to the datagram's source.
    Answer(Box<SipResponse>),
    /// A 400 could answer it, but the cap is spent: dropped.
    Capped,
    /// No response answers it (a response, an ACK, a request whose echoed
    /// fields do not read, a limit): dropped.
    Dropped,
}

/// One ingress's cap on the 400s answering refused requests: a full bucket of
/// [`PARSE_REFUSAL_BURST`] tokens refilling at [`PARSE_REFUSAL_PER_SEC`].
#[derive(Debug, Clone)]
pub struct ParseRefusalCap {
    bucket: TokenBucket,
}

impl ParseRefusalCap {
    /// A full cap, its time credited up to `now` on the caller's timeline.
    pub fn new(now: Duration) -> Self {
        Self {
            bucket: TokenBucket::full(
                f64::from(PARSE_REFUSAL_BURST),
                f64::from(PARSE_REFUSAL_PER_SEC),
                now,
            ),
        }
    }

    /// Take one token at `now`; whether there was one.
    pub fn try_take(&mut self, now: Duration) -> bool {
        self.bucket.try_take(now).is_ok()
    }
}

/// The fate of the datagram from `source` that `error` refused. Without a
/// partial it is dropped before `take` is asked for a token; with one and a
/// token, it is answered under the To-tag `tagger` derives from the request
/// (§8.2.6.2), naming `agent()` in the Warning.
pub fn refuse(
    error: &SipParseError,
    source: SocketAddr,
    tagger: &StatelessTagger,
    agent: impl FnOnce() -> String,
    take: impl FnOnce() -> bool,
) -> ParseRefusal {
    let Some(partial) = error.partial() else { return ParseRefusal::Dropped };
    if !take() {
        return ParseRefusal::Capped;
    }
    let to_tag = tagger.for_partial(partial).0;
    let source = sip_message::received::observed_source(source);
    match generate_invalid_request(partial, &error.reason, &agent(), &to_tag, &source) {
        Ok(response) => ParseRefusal::Answer(Box::new(response)),
        Err(_) => ParseRefusal::Dropped,
    }
}
