//! The continuation codec: the peer may return the opaque value under a
//! peer-specific wrapping. The codec wraps the token on emission and unwraps
//! candidates from a request body on the scan; the service holds no progress
//! either way (the unwrapped token is the position). A wrapping may echo fields
//! of the reply it stands in, never state of its own.

/// How a continuation token travels in a peer's payload.
///
/// [`wrap`](Self::wrap)'s output stands verbatim where a reply template says
/// `${continuation}`, so it must need no escaping in the surrounding payload.
/// It reads the reply's body rendered with every `${continuation}` empty, so
/// a peer whose context mirrors its own answer can be reproduced.
/// [`unwrap`](Self::unwrap) returns the texts the service scans for tokens
/// besides the raw body, which is always scanned: an unwrapped candidate that
/// holds no valid token is the peer's data. Request fragments (`contains`)
/// always match the body as sent, never an unwrapped candidate.
pub trait HttpContinuationCodec: Send + Sync {
    /// The text standing for `token` in the reply whose body, rendered with
    /// every `${continuation}` empty, is `reply`.
    fn wrap(&self, token: &str, reply: &str) -> String;
    /// The unwrapped texts of the values in `body` that may carry a token.
    fn unwrap(&self, body: &[u8]) -> Vec<String>;
}

/// The default: the token travels as minted, found by the raw scan alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityCodec;

impl HttpContinuationCodec for IdentityCodec {
    fn wrap(&self, token: &str, _reply: &str) -> String {
        token.to_string()
    }

    fn unwrap(&self, _body: &[u8]) -> Vec<String> {
        Vec::new()
    }
}
