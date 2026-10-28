//! `X-Worker-Dead`: what a front proxy states, on an out-of-dialog OPTIONS to
//! a worker, about another worker its health view reads dead (ADR-0050 §3).
//!
//! ```text
//! X-Worker-Dead: b1; at=1792155573698; sig=Ab3…
//! ```
//!
//! The value names the dead worker's ordinal (a `token`, RFC 3261 §25.1), the
//! instant of the verdict in epoch milliseconds, and the signature the proxy
//! made of both, base64url without padding. This module only reads and
//! writes the text; the signature is the caller's to make and to check, over
//! [`DeadNotice::signed_fields`].

use base64::Engine as _;

use crate::char_class::is_token_char;
use crate::header::HeaderName;
use crate::types::SipRequest;

/// The wire name of the header.
pub const X_WORKER_DEAD: &str = "X-Worker-Dead";

/// The most bytes a header value may hold.
pub const MAX_LEN: usize = 256;

/// The fixed first field every signature covers, so a signature made for
/// another statement under the same key never verifies as a notice.
const DOMAIN: &[u8] = b"x-worker-dead/1";

/// A proxy's verdict that a worker is dead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadNotice {
    /// The ordinal of the worker read dead.
    pub ordinal: String,
    /// When the proxy read it dead, in epoch milliseconds.
    pub at_ms: i64,
}

/// A notice as it arrived: the signature it carries, if any, still unchecked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedNotice {
    pub notice: DeadNotice,
    pub signature: Option<Vec<u8>>,
}

impl DeadNotice {
    /// The fields the signature covers, in order.
    pub fn signed_fields(&self) -> [Vec<u8>; 3] {
        [DOMAIN.to_vec(), self.ordinal.as_bytes().to_vec(), self.at_ms.to_string().into_bytes()]
    }

    /// The header value carrying the notice and `signature`; `None` when the
    /// ordinal is not a token or the value would exceed [`MAX_LEN`].
    pub fn header_value(&self, signature: &[u8]) -> Option<String> {
        if !is_ordinal(&self.ordinal) {
            return None;
        }
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature);
        let value = format!("{}; at={}; sig={sig}", self.ordinal, self.at_ms);
        (value.len() <= MAX_LEN).then_some(value)
    }

    /// Read one header value. `None` when it exceeds [`MAX_LEN`], names no
    /// token ordinal, or states no integer `at`; a `sig` that is not
    /// base64url reads as no signature. Parameter names are case-insensitive.
    pub fn parse(value: &str) -> Option<ReceivedNotice> {
        if value.len() > MAX_LEN {
            return None;
        }
        let mut parts = value.split(';');
        let ordinal = parts.next()?.trim();
        if !is_ordinal(ordinal) {
            return None;
        }
        let (mut at_ms, mut signature) = (None, None);
        for param in parts {
            let Some((name, val)) = param.split_once('=') else { continue };
            let (name, val) = (name.trim(), val.trim());
            if name.eq_ignore_ascii_case("at") {
                at_ms = val.parse::<i64>().ok();
            } else if name.eq_ignore_ascii_case("sig") {
                signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(val).ok();
            }
        }
        Some(ReceivedNotice {
            notice: DeadNotice { ordinal: ordinal.to_string(), at_ms: at_ms? },
            signature,
        })
    }
}

/// Whether `ordinal` is a non-empty `token`.
fn is_ordinal(ordinal: &str) -> bool {
    !ordinal.is_empty() && ordinal.bytes().all(is_token_char)
}

/// The notice the first `X-Worker-Dead` of `req` carries; `None` when absent
/// or unreadable. A later instance is never read: a request states one
/// verdict.
pub fn dead_notice(req: &SipRequest) -> Option<ReceivedNotice> {
    req.raw(HeaderName::from(X_WORKER_DEAD)).next().and_then(DeadNotice::parse)
}

#[cfg(test)]
mod tests;
